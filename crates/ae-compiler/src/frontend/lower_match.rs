//! Lowering module: match (P1.4 parser split).
use super::*;

impl<'a> Parser<'a> {
    // `expr?` propagates the error variant of a result enum: on `Err` every
    // live owned value is released and the enum itself is returned, so a caller
    // never observes a half-finished result. On `Ok` the payload is bound (or
    // released when the statement only wanted the side effect).
    pub(super) fn propagate_statement(&mut self, value: Expr, ty: Type, binding: Option<usize>) -> Result<(Statement, Option<Type>), ae_span::Diagnostic> {
        let eid = match ty {
            Type::Enum(_, eid) => eid,
            _ => return Err(self.fail(&format!("'?' needs a result or option value, found {}", ty.name(self.ctx)))),
        };
        let def = self.enums[eid].clone();
        // Both two-variant shapes can be propagated: a result hands its Err
        // back, an Option hands its None back. Neither variant has to be a
        // prelude enum - a user enum with the same shape behaves identically.
        let result_shaped = def.variants.len() == 2
            && def.variants[0].0 == "Ok" && def.variants[1].0 == "Err"
            && def.variants[1].1 == [Type::String];
        let option_shaped = def.variants.len() == 2
            && def.variants[0].0 == "Some" && def.variants[1].0 == "None"
            && def.variants[1].1.is_empty() && def.variants[0].1.len() == 1;
        if !result_shaped && !option_shaped {
            return Err(self.fail(&format!("'?' needs an enum shaped {{ Ok(...), Err(string) }} or {{ Some(T), None }}, and '{}' is not one", def.name)));
        }
        if self.in_main || self.return_type != ty {
            return Err(self.fail(&format!("'?' needs the enclosing function to return {}, so the failure can be handed on", def.name)));
        }
        let fields = def.variants[0].1.clone();
        let bound = match binding {
            Some(id) => {
                if fields.len() == 1 && fields[0] == Type::Unit {
                    return Err(self.fail(&format!("'{}::{}' carries no value; use a bare `?`", def.name, def.variants[0].0)));
                }
                if fields.len() != 1 {
                    return Err(self.fail(&format!("'{}::{}' carries {} values; a binding `let` needs exactly one (use a bare statement to discard)", def.name, def.variants[0].0, fields.len())));
                }
                Some((id, fields[0]))
            }
            None => None,
        };
        let bound_ty = bound.map(|(_, ty)| ty);
        Ok((Statement::Propagate(value, ty, fields, bound, self.live_owned()), bound_ty))
    }

    pub(super) fn lower_match(
        &mut self,
        subject: Expr,
        subject_ty: Type,
        mut lower_body: impl FnMut(&mut Self, usize, &[(String, usize)]) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        let (sid, eid) = match (&subject, subject_ty) {
            (Expr::Variable(id, Type::Enum(_, _), _, _), Type::Enum(_, k)) => (*id, k),
            _ => return Err(self.fail("match needs a bare enum variable as its subject")),
        };
        if self.moved.contains(&sid) { return Err(self.fail("cannot match a value that was moved")); }
        let def = self.enums[eid].clone();
        self.take(Kind::LBrace)?;
        let mut arms: Vec<MatchArm> = Vec::new();
        let mut seen_wildcard = false;
        self.borrows.push((sid, self.scopes.len(), BorrowKind::Container));
        while *self.current() != Kind::RBrace {
            // A bare name (other than `_`) cannot reach a variant: keep the
            // specific remedy instead of the shared parser's generic separator error.
            if matches!(self.current(), Kind::Ident(name) if name != "_")
                && self.tokens.get(self.pos + 1).is_some_and(|token| token.kind != Kind::ColonColon)
            {
                return Err(self.fail(&format!("pattern must name the matched enum ('{}::Variant')", def.pattern)));
            }
            let offset = self.tokens[self.pos].offset;
            let (tree, end) = if let Some(pattern) = self.resolved_expressions().match_patterns.get(&offset).cloned() {
                let end = self.tokens.partition_point(|token| token.offset < pattern.range.end);
                (pattern, end)
            } else {
                ae_syntax::pattern::parse_pattern_tokens(&self.file, self.source, &self.tokens, self.pos)?
            };
            self.pos = end;
            if seen_wildcard {
                return Err(self.error_at(tree.range.start, "unreachable match arm after wildcard '_'"));
            }
            self.scopes.push(Vec::new());
            let mut bindings = Vec::new();
            let mut bound = Vec::new();
            let mut path = Vec::new();
            let compiled_pattern = self.lower_pattern_node(
                &tree,
                subject_ty,
                &mut path,
                &mut bindings,
                &mut bound,
                true,
            )?;
            let seen_matrix: Vec<Vec<&CompiledPattern>> = arms.iter().map(|arm| vec![&arm.pattern]).collect();
            match &compiled_pattern {
                CompiledPattern::Wildcard => {
                    if !is_useful(&seen_matrix, &[&CompiledPattern::Wildcard], self.enums) {
                        return Err(self.error_at(
                            tree.range.start,
                            "wildcard '_' is redundant: all variants are already covered",
                        ));
                    }
                    seen_wildcard = true;
                }
                CompiledPattern::Variant { variant, .. } => {
                    if !is_useful(&seen_matrix, &[&compiled_pattern], self.enums) {
                        let (vname, vstart) = match &tree.kind {
                            ae_syntax::pattern::PatternKind::Variant { variant, variant_range, .. } => {
                                (variant.as_str(), variant_range.start)
                            }
                            _ => ("", tree.range.start),
                        };
                        if is_flat_pattern(&compiled_pattern)
                            && arms.iter().any(|arm| {
                                is_flat_pattern(&arm.pattern)
                                    && matches!(arm.pattern, CompiledPattern::Variant { variant: v, .. } if v == *variant)
                            })
                        {
                            return Err(self.error_at(
                                vstart,
                                &format!("variant '{}::{}' is matched twice", def.name, vname),
                            ));
                        }
                        return Err(self.error_at(
                            vstart,
                            &format!("unreachable match arm: '{}::{}' is already covered", def.name, vname),
                        ));
                    }
                }
                CompiledPattern::Binding => {}
            }
            self.take(Kind::FatArrow)?;
            let body = lower_body(self, arms.len(), &bound)?;
            self.scopes.pop().unwrap();
            for (name, _) in bound { self.names.remove(&name); }
            arms.push(MatchArm { pattern: compiled_pattern, bindings, body });
        }
        self.take(Kind::RBrace)?;
        self.borrows.pop();
        let seen_matrix: Vec<Vec<&CompiledPattern>> = arms.iter().map(|arm| vec![&arm.pattern]).collect();
        if arms.is_empty() || is_useful(&seen_matrix, &[&CompiledPattern::Wildcard], self.enums) {
            let missing = missing_witness(&seen_matrix, eid, self.enums)
                .unwrap_or_else(|| format!("{}::{}", def.name, def.variants[0].0));
            return Err(self.fail(&format!("match is not exhaustive: '{missing}' is not handled")));
        }
        Ok(Statement::Match(sid, subject_ty, arms))
    }

    fn lower_pattern_node(
        &mut self,
        pattern: &ae_syntax::pattern::Pattern,
        expected_ty: Type,
        path: &mut Vec<(usize, usize)>,
        bindings: &mut Vec<MatchBinding>,
        bound: &mut Vec<(String, usize)>,
        is_top_level: bool,
    ) -> Result<CompiledPattern, ae_span::Diagnostic> {
        match &pattern.kind {
            ae_syntax::pattern::PatternKind::Wildcard => Ok(CompiledPattern::Wildcard),
            ae_syntax::pattern::PatternKind::Binding(bname) => {
                if is_top_level {
                    return Err(ae_syntax::diagnostic_range(
                        &self.file,
                        self.source,
                        pattern.range.clone(),
                        "expected a qualified variant pattern",
                    ));
                }
                let name_at = pattern.range.start;
                if RESERVED.contains(&bname.as_str()) {
                    return Err(self.error_at(
                        name_at,
                        &format!("'{bname}' is a built-in name and cannot be a pattern binding"),
                    ));
                }
                if self.names.contains_key(bname) {
                    return Err(self.duplicate_binding_error_at(name_at, bname));
                }
                let id = self.next_var;
                self.next_var += 1;
                // String payloads bind as read-only views and vec payloads
                // as read-only borrows: nothing can be moved out of the
                // matched enum. A struct payload (ADR-0234) binds the same
                // way — the enum keeps ownership for its release.
                let bty = match expected_ty {
                    Type::String => Type::StrView,
                    Type::Vector(vid) => Type::VectorRef(vid),
                    Type::Struct(k) => Type::StructRef(k),
                    other => other,
                };
                self.names.insert(bname.clone(), (id, false, bty));
                // The binding's own range: a closure that captures this
                // payload needs the outer start to resolve its definition.
                self.binding_ranges.insert(id, name_at..name_at + bname.len());
                // Non-Copy enum payloads bound directly must not be freed at
                // arm exit because the matched outer enum retains ownership.
                if !matches!(bty, Type::Enum(_, k) if !self.enums[k].is_copy) {
                    self.scopes.last_mut().unwrap().push((id, bty));
                }
                bound.push((bname.clone(), name_at));
                bindings.push((id, expected_ty, path.clone()));
                Ok(CompiledPattern::Binding)
            }
            ae_syntax::pattern::PatternKind::Variant { enum_name, variant: vname, variant_range, payload } => {
                let eid = match expected_ty {
                    Type::Enum(_, k) => k,
                    _ => {
                        return Err(self.error_at(
                            pattern.range.start,
                            "pattern payloads must be plain names",
                        ));
                    }
                };
                let def = self.enums[eid].clone();
                // An instantiated enum is matched by its declaration name:
                // `Option::Some(v)` covers Option[string] and Option[i64] alike.
                // A preset family unified with its `Result[p, string]`
                // instantiation also answers to that generic name (P2.4).
                if *enum_name != def.pattern && !def.aliases.iter().any(|alias| *alias == *enum_name) {
                    return Err(self.error_at(
                        pattern.range.start,
                        &format!("pattern must name the matched enum ('{}::Variant')", def.pattern),
                    ));
                }
                let vidx = def.variants.iter().position(|(n, _)| *n == *vname)
                    .ok_or_else(|| self.error_at(
                        variant_range.start,
                        &format!("enum '{}' has no variant '{}'", def.name, vname),
                    ))?;
                let fields = def.variants[vidx].1.clone();
                // A unit payload carries no value, so the arm writes the variant
                // alone (`Result::Ok =>`) — the same form an empty payload takes.
                let unit_payload = fields.len() == 1 && fields[0] == Type::Unit;
                // The declaration decides the payload shape; `()` is a parse-level
                // empty list, so an empty or mismatched list lands here.
                let payloads = match payload {
                    Some(_) if fields.is_empty() => {
                        return Err(self.error_at(
                            variant_range.end,
                            &format!("variant '{}::{}' has no payload", def.name, vname),
                        ));
                    }
                    Some(patterns) if unit_payload => {
                        return Err(self.error_at(
                            patterns[0].range.start,
                            &format!("variant '{}::{}' carries no value; write the arm without bindings", def.name, vname),
                        ));
                    }
                    None if !fields.is_empty() && !unit_payload => {
                        return Err(self.error_at(
                            variant_range.start,
                            &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len()),
                        ));
                    }
                    Some(patterns) if patterns.len() > fields.len() => {
                        return Err(self.error_at(
                            patterns[fields.len()].range.start,
                            &format!("variant '{}::{}' binds {} payload value(s)", def.name, vname, fields.len()),
                        ));
                    }
                    Some(patterns) if patterns.len() < fields.len() => {
                        return Err(self.error_at(
                            variant_range.start,
                            &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len()),
                        ));
                    }
                    Some(patterns) => patterns.as_slice(),
                    None => &[],
                };
                let mut compiled_fields = Vec::with_capacity(fields.len());
                for (index, (field_ty, subpat)) in fields.iter().copied().zip(payloads).enumerate() {
                    path.push((vidx, index));
                    let compiled_sub = self.lower_pattern_node(
                        subpat,
                        field_ty,
                        path,
                        bindings,
                        bound,
                        false,
                    )?;
                    path.pop();
                    compiled_fields.push((index, field_ty, compiled_sub));
                }
                Ok(CompiledPattern::Variant {
                    eid,
                    variant: vidx,
                    fields: compiled_fields,
                })
            }
        }
    }

}

fn is_flat_pattern(pattern: &CompiledPattern) -> bool {
    match pattern {
        CompiledPattern::Wildcard | CompiledPattern::Binding => true,
        CompiledPattern::Variant { fields, .. } => {
            fields.iter().all(|(_, _, sub)| matches!(sub, CompiledPattern::Wildcard | CompiledPattern::Binding))
        }
    }
}

fn specialize_row<'a>(row: &[&'a CompiledPattern], variant: usize, arity: usize) -> Option<Vec<&'a CompiledPattern>> {
    let first = row.first()?;
    let mut out = Vec::new();
    match first {
        CompiledPattern::Wildcard | CompiledPattern::Binding => {
            static WILDCARD: CompiledPattern = CompiledPattern::Wildcard;
            for _ in 0..arity {
                out.push(&WILDCARD);
            }
        }
        CompiledPattern::Variant { variant: v, fields, .. } => {
            if *v != variant {
                return None;
            }
            for (_, _, sub) in fields {
                out.push(sub);
            }
        }
    }
    out.extend_from_slice(&row[1..]);
    Some(out)
}

fn specialize_matrix<'a>(matrix: &[Vec<&'a CompiledPattern>], variant: usize, arity: usize) -> Vec<Vec<&'a CompiledPattern>> {
    matrix.iter().filter_map(|row| specialize_row(row, variant, arity)).collect()
}

fn default_matrix<'a>(matrix: &[Vec<&'a CompiledPattern>]) -> Vec<Vec<&'a CompiledPattern>> {
    matrix
        .iter()
        .filter_map(|row| match row.first() {
            Some(CompiledPattern::Wildcard | CompiledPattern::Binding) => Some(row[1..].to_vec()),
            _ => None,
        })
        .collect()
}

fn is_useful(matrix: &[Vec<&CompiledPattern>], row: &[&CompiledPattern], enums: &[EnumDef]) -> bool {
    if row.is_empty() {
        return matrix.is_empty();
    }
    if matrix.is_empty() {
        return true;
    }
    match row[0] {
        CompiledPattern::Variant { variant, fields, .. } => {
            let sub_matrix = specialize_matrix(matrix, *variant, fields.len());
            if let Some(sub_row) = specialize_row(row, *variant, fields.len()) {
                is_useful(&sub_matrix, &sub_row, enums)
            } else {
                false
            }
        }
        CompiledPattern::Wildcard | CompiledPattern::Binding => {
            let mut head_eid: Option<usize> = None;
            let mut seen_variants = std::collections::HashSet::new();
            for r in matrix {
                if let Some(CompiledPattern::Variant { eid, variant, .. }) = r.first() {
                    head_eid = Some(*eid);
                    seen_variants.insert(*variant);
                }
            }
            let Some(eid) = head_eid else {
                return is_useful(&default_matrix(matrix), &row[1..], enums);
            };
            let variants = &enums[eid].variants;
            if seen_variants.len() == variants.len() {
                variants.iter().enumerate().any(|(vidx, (_, field_types))| {
                    let arity = if field_types.len() == 1 && field_types[0] == Type::Unit { 0 } else { field_types.len() };
                    let sub_matrix = specialize_matrix(matrix, vidx, arity);
                    if let Some(sub_row) = specialize_row(row, vidx, arity) {
                        is_useful(&sub_matrix, &sub_row, enums)
                    } else {
                        false
                    }
                })
            } else {
                is_useful(&default_matrix(matrix), &row[1..], enums)
            }
        }
    }
}

fn missing_witness(matrix: &[Vec<&CompiledPattern>], eid: usize, enums: &[EnumDef]) -> Option<String> {
    let variants = &enums[eid].variants;
    let mut seen_variants = std::collections::HashSet::new();
    for r in matrix {
        match r.first() {
            Some(CompiledPattern::Wildcard | CompiledPattern::Binding) => return None,
            Some(CompiledPattern::Variant { variant, .. }) => {
                seen_variants.insert(*variant);
            }
            None => {}
        }
    }
    for (vidx, (vname, _)) in variants.iter().enumerate() {
        if !seen_variants.contains(&vidx) {
            return Some(format!("{}::{}", enums[eid].name, vname));
        }
    }
    for (vidx, (vname, field_types)) in variants.iter().enumerate() {
        let arity = if field_types.len() == 1 && field_types[0] == Type::Unit { 0 } else { field_types.len() };
        let sub_matrix = specialize_matrix(matrix, vidx, arity);
        let row: Vec<&CompiledPattern> = vec![&CompiledPattern::Wildcard; arity];
        if is_useful(&sub_matrix, &row, enums) {
            let mut parts = Vec::with_capacity(arity);
            for (fidx, fty) in field_types.iter().take(arity).enumerate() {
                if let Type::Enum(_, sub_eid) = *fty {
                    let col_matrix: Vec<Vec<&CompiledPattern>> = sub_matrix
                        .iter()
                        .filter_map(|r| r.get(fidx).map(|p| vec![*p]))
                        .collect();
                    if let Some(sub_witness) = missing_witness(&col_matrix, sub_eid, enums) {
                        parts.push(sub_witness);
                        continue;
                    }
                }
                parts.push("_".to_owned());
            }
            if parts.is_empty() {
                return Some(format!("{}::{}", enums[eid].name, vname));
            }
            return Some(format!("{}::{}({})", enums[eid].name, vname, parts.join(", ")));
        }
    }
    None
}
