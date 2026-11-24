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
        let mut seen = vec![false; def.variants.len()];
        self.borrows.push((sid, self.scopes.len(), BorrowKind::Container));
        while *self.current() != Kind::RBrace {
            // A bare name cannot reach a variant: keep the specific remedy
            // instead of the shared parser's generic separator error.
            if matches!(self.current(), Kind::Ident(_))
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
            let ae_syntax::pattern::PatternKind::Variant {
                enum_name, variant: vname, variant_range, payload,
            } = tree.kind
            else {
                return Err(ae_syntax::diagnostic_range(
                    &self.file, self.source, tree.range, "expected a qualified variant pattern",
                ));
            };
            // An instantiated enum is matched by its declaration name:
            // `Option::Some(v)` covers Option[string] and Option[i64] alike.
            // A preset family unified with its `Result[p, string]`
            // instantiation also answers to that generic name (P2.4).
            if enum_name != def.pattern && !def.aliases.iter().any(|alias| *alias == enum_name) {
                return Err(self.error_at(tree.range.start,
                    &format!("pattern must name the matched enum ('{}::Variant')", def.pattern)));
            }
            let vidx = def.variants.iter().position(|(n, _)| *n == vname)
                .ok_or_else(|| self.error_at(variant_range.start,
                    &format!("enum '{}' has no variant '{}'", def.name, vname)))?;
            if seen[vidx] {
                return Err(self.error_at(variant_range.start,
                    &format!("variant '{}::{}' is matched twice", def.name, vname)));
            }
            seen[vidx] = true;
            let fields = def.variants[vidx].1.clone();
            // A unit payload carries no value, so the arm writes the variant
            // alone (`Result::Ok =>`) — the same form an empty payload takes.
            let unit_payload = fields.len() == 1 && fields[0] == Type::Unit;
            // The declaration decides the payload shape; `()` is a parse-level
            // empty list, so an empty or mismatched list lands here.
            let payloads = match payload {
                Some(_) if fields.is_empty() => {
                    return Err(self.error_at(variant_range.end,
                        &format!("variant '{}::{}' has no payload", def.name, vname)));
                }
                Some(patterns) if unit_payload => {
                    return Err(self.error_at(patterns[0].range.start,
                        &format!("variant '{}::{}' carries no value; write the arm without bindings", def.name, vname)));
                }
                None if !fields.is_empty() && !unit_payload => {
                    return Err(self.error_at(variant_range.start,
                        &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len())));
                }
                Some(patterns) if patterns.len() > fields.len() => {
                    return Err(self.error_at(patterns[fields.len()].range.start,
                        &format!("variant '{}::{}' binds {} payload value(s)", def.name, vname, fields.len())));
                }
                Some(patterns) if patterns.len() < fields.len() => {
                    return Err(self.error_at(variant_range.start,
                        &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len())));
                }
                Some(patterns) => patterns,
                None => Vec::new(),
            };
            self.scopes.push(Vec::new());
            let mut bindings = Vec::new();
            let mut bound = Vec::new();
            for (index, (field, payload_pattern)) in fields.iter().copied().zip(payloads).enumerate() {
                let bname = match payload_pattern.kind {
                    ae_syntax::pattern::PatternKind::Binding(name) => name,
                    ae_syntax::pattern::PatternKind::Variant { .. } => {
                        return Err(self.error_at(payload_pattern.range.start,
                            "pattern payloads must be plain names"));
                    }
                };
                let name_at = payload_pattern.range.start;
                if RESERVED.contains(&bname.as_str()) {
                    return Err(self.error_at(name_at,
                        &format!("'{bname}' is a built-in name and cannot be a pattern binding")));
                }
                if self.names.contains_key(&bname) {
                    return Err(self.duplicate_binding_error_at(name_at, &bname));
                }
                let id = self.next_var; self.next_var += 1;
                // String payloads bind as read-only views and vec payloads
                // as read-only borrows: nothing can be moved out of the
                // matched enum.
                let bty = match field {
                    Type::String => Type::StrView,
                    Type::Vec => Type::VecRef,
                    Type::StrVec => Type::StrVecRef,
                    Type::VecF => Type::VecFRef,
                    other => other,
                };
                self.names.insert(bname.clone(), (id, false, bty));
                // The binding's own range: a closure that captures this
                // payload needs the outer start to resolve its definition.
                self.binding_ranges.insert(id, name_at..name_at + bname.len());
                self.scopes.last_mut().unwrap().push((id, bty));
                bound.push((bname, name_at));
                bindings.push((id, field, index));
            }
            self.take(Kind::FatArrow)?;
            let body = lower_body(self, arms.len(), &bound)?;
            self.scopes.pop().unwrap();
            for (name, _) in bound { self.names.remove(&name); }
            arms.push(MatchArm { variant: vidx, bindings, body });
        }
        self.take(Kind::RBrace)?;
        self.borrows.pop();
        if let Some(missing) = seen.iter().position(|covered| !covered) {
            return Err(self.fail(&format!("match is not exhaustive: '{}::{}' is not handled", def.name, def.variants[missing].0)));
        }
        Ok(Statement::Match(sid, subject_ty, arms))
    }

}
