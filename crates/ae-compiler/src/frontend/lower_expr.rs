//! Lowering module: expr (P1.4 parser split).
use super::*;

fn constant_int_literal(expr: &Expr) -> Option<i128> {
    match expr {
        Expr::Number(value) => Some(*value as i128),
        Expr::Neg(inner, ..) => match &**inner {
            Expr::Number(value) => Some(-(*value as i128)),
            _ => None,
        },
        _ => None,
    }
}
use super::parser::{moved_value_hint, SharedExpressionEffects};
use super::route_marks::*;

impl<'a> Parser<'a> {
    pub(super) fn lower_scalar_body_expression(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        Ok(match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => (Expr::Number(*value), Type::Int(IntTy::I64)),
            ExpressionKind::Atom(Kind::Float(value)) => (Expr::FloatLit(*value), Type::Float(FloatTy::F64)),
            ExpressionKind::Atom(Kind::Char(value)) => (Expr::CharLit(*value), Type::Char),
            ExpressionKind::Atom(Kind::True) => (Expr::BoolLit(true), Type::Bool),
            ExpressionKind::Atom(Kind::False) => (Expr::BoolLit(false), Type::Bool),
            ExpressionKind::Unit => (Expr::Unit, Type::Unit),
            ExpressionKind::Atom(Kind::Text(value)) => {
                self.seek_scalar_offset(expression.range.start);
                let (line, column) = self.position();
                (Expr::StrLit(value.clone(), line, column), Type::String)
            }
            ExpressionKind::Atom(Kind::Ident(name)) => {
                // A name the resolver did not record is a diagnostic, never a
                // panic: a permissive scan can hand us a malformed tree. The
                // classification mirrors the typed ident arm: a struct name
                // gets its construction remedy (the cursor sits after the
                // name, where the typed arm's `fail` lands), everything else is
                // an unknown variable at the name itself.
                let def = self.resolved_expressions().scalar_reads.get(&expression.range.start).copied();
                if let Some(def) = def
                    && let Some(&(slot, ty)) = slots.get(&def)
                {
                    self.seek_scalar_offset(expression.range.start);
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"),
                            slot,
                        )));
                    }
                    let (line, column) = self.position();
                    (Expr::Variable(slot, ty, line, column), ty)
                } else {
                    if !self.names.contains_key(name) {
                        if self.generics.fn_names.contains_key(name) {
                            return Err(self.error_at(expression.range.start, &format!("generic function '{name}' cannot be used as a function value")));
                        }
                        if let Some(function) = self.functions.get(name).cloned() {
                            if !function.public && function.file != self.file {
                                return Err(self.error_at(expression.range.start, &format!("function '{name}' is private to {}", function.file)));
                            }
                            if function.extern_c || self.ctx.is_unsafe_fn(function.id) {
                                return Err(self.error_at(expression.range.start, &format!("function '{name}' cannot be passed as a safe function value")));
                            }
                            if matches!(function.ret, Type::StructRef(_) | Type::StructMut(_)) {
                                return Err(self.error_at(expression.range.start, "a function value cannot return a borrow"));
                            }
                            let sig = self.ctx.intern_fn_sig(function.params, function.ret);
                            return Ok((Expr::FnRef(function.id, sig), Type::Fn(sig)));
                        }
                    }
                    if def.is_none() {
                        if self.generics.names.contains_key(name) {
                            return Err(self.error_at(expression.range.start, &format!("'{name}' is a generic enum; write '{name}[string]::Variant'")));
                        }
                        if self.struct_names.contains_key(name) {
                            self.seek_scalar_offset(expression.range.end);
                            return Err(self.fail(&format!("'{name}' is a struct name; construct it with '{name} {{ ... }}'")));
                        }
                    }
                    return Err(self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101"));
                }
            }
            ExpressionKind::Group(inner) => self.lower_scalar_body_expression(inner, slots)?,
            ExpressionKind::Closure { .. } => {
                self.seek_scalar_offset(expression.range.start);
                let lowered = self.closure_literal()?;
                self.seek_scalar_offset(expression.range.end);
                lowered
            }
            ExpressionKind::AddressCast { binding, binding_range, mutable, .. } => {
                self.seek_scalar_offset(expression.range.start);
                let position = self.position();
                let (slot, ty) = self.resolved_slot(
                    self.resolved_expressions().scalar_reads.get(&binding_range.start),
                    slots, binding_range.start, binding)?;
                self.seek_scalar_offset(binding_range.start);
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot use '{binding}' because it was moved")).with_code("E0102"), slot,
                    )));
                }
                let (line, column) = self.position();
                let operand = Expr::Variable(slot, ty, line, column);
                self.seek_scalar_offset(binding_range.end);
                self.check_address_binding(slot, *mutable)?;
                self.take(Kind::As)?;
                let (_, pointee) = self.pointer_cast_pointee(Some(*mutable))?;
                self.lower_address_cast(operand, ty, pointee, *mutable, position)?
            }
            // `raw as *mut f64` (ADR-0209 step A): the address stays, the
            // pointee changes. The base must already be a pointer — value
            // conversions are `to_f64`/`to_i64`'s territory — and the pointee
            // spelling comes from the shared node's range.
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                self.seek_scalar_offset(expression.range.start);
                let (operand, base_type) = self.lower_scalar_body_expression(base, slots)?;
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_) | Type::Int(IntTy::Usize)) {
                    self.seek_scalar_offset(expression.range.start);
                    return Err(self.fail(&format!("'as' retags a raw pointer, and {} is not a pointer", base_type.name(self.ctx))));
                }
                if base_type == Type::Int(IntTy::Usize) && self.unsafe_depth == 0 {
                    return Err(self.error_at(expression.range.start, "integer-to-pointer cast requires 'unsafe'").with_code("E0701"));
                }
                let name = self.source.get(pointee.clone())
                    .ok_or_else(|| self.error_at(pointee.start, "cannot read the pointer's pointee type"))?;
                let pointee_type = self.subst.get(name).copied()
                    .or_else(|| super::types::scalar_type(name))
                    // spec/unsafe.md 2.1 promises a retag to any T with a
                    // known layout: a `#[repr(C)]` struct qualifies (its
                    // fields are scalars or other repr(C) structs, so a read
                    // cannot duplicate an owned value). A tracked layout
                    // stays out — its bytes cannot gain a second owner.
                    .or_else(|| self.struct_names.get(name).and_then(|&sid| {
                        self.structs[sid].repr_c.then_some(Type::Struct(sid))
                    }))
                    .ok_or_else(|| self.error_at(pointee.start, "a pointer cast retags to a scalar pointee here"))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end).ok_or_else(|| self.fail("expected the end of the pointer cast"))?;
                let id = self.ctx.intern_pointee(pointee_type);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                (Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty)
            }
            ExpressionKind::ScalarCast { base, target } => {
                self.seek_scalar_offset(expression.range.start);
                let (operand, from) = self.lower_scalar_body_expression(base, slots)?;
                let name = self.source.get(target.clone())
                    .ok_or_else(|| self.error_at(target.start, "cannot read the cast's target type"))?;
                let to = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))
                    .ok_or_else(|| self.error_at(target.start, &format!("'{name}' is not a scalar type")))?;
                if matches!(from, Type::Ptr(_) | Type::PtrMut(_)) && to == Type::Int(IntTy::Usize) {
                    if self.unsafe_depth == 0 {
                        return Err(self.error_at(expression.range.start, "pointer-to-integer cast requires 'unsafe'").with_code("E0701"));
                    }
                    let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                    let token = self.tokens.get(end).ok_or_else(|| self.fail("expected the end of the cast"))?;
                    return Ok((Expr::ScalarCast(Box::new(operand), from, to, token.line, token.column), to));
                }
                let valid = match (from.scalar(), to.scalar()) {
                    (Some(from_scalar), Some(to_scalar)) => ae_ty::valid_scalar_cast(from_scalar, to_scalar),
                    _ => false,
                };
                if !valid {
                    self.seek_scalar_offset(target.start);
                    return Err(self.fail(&format!("conversion from {} to {name} is not allowed", from.name(self.ctx))));
                }
                if let Type::Int(target_int) = to
                    && let Some(value) = constant_int_literal(&operand)
                    && !target_int.contains(value)
                {
                    let (min, max) = target_int.range();
                    self.seek_scalar_offset(target.start);
                    return Err(self.fail(&format!(
                        "integer literal {value} is out of range for '{}' ({min}..={max})",
                        target_int.name(),
                    )));
                }
                if from == to {
                    return Ok((operand, to));
                }
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end).ok_or_else(|| self.fail("expected the end of the cast"))?;
                (Expr::ScalarCast(Box::new(operand), from, to, token.line, token.column), to)
            }
            ExpressionKind::Struct { target, fields } => {
                // Validate in source order and emit in layout order.
                let sid = match &target.kind {
                    ExpressionKind::Atom(Kind::Ident(name)) => {
                        if name == "Self" {
                            // `Self { .. }` inside a concrete impl body builds
                            // the impl target's struct layout.
                            match self.impl_self {
                                Some(Type::Struct(sid)) => sid,
                                _ => return Err(self.fail("'Self' does not name a struct here; write the struct's name")),
                            }
                        } else {
                            // A permissive scan can hand a construction whose
                            // name the registry does not hold (a mutant, or a
                            // name bound only as a value): the typed route
                            // reads it as a variable, so report the same.
                            match self.struct_names.get(name) {
                                Some(&sid) => sid,
                                None if self.generics.struct_names.contains_key(name) => {
                                    let gid = self.generics.struct_names[name];
                                    let gdef = self.generics.struct_defs[gid].clone();
                                    let mut field_vals: Vec<Option<(Expr, Type)>> = vec![None; gdef.fields.len()];
                                    let mut inferred: Vec<Option<Type>> = vec![None; gdef.params.len()];
                                    let mut has_closure = false;
                                    for field in fields {
                                        let Some(pos) = gdef.fields.iter().position(|(fnm, _)| fnm == &field.name) else {
                                            return Err(self.error_at(field.name_range.start, &format!("struct '{}' has no field '{}'", gdef.name, field.name)));
                                        };
                                        if field_vals[pos].is_some() {
                                            return Err(self.error_at(field.name_range.start, &format!("duplicate field '{}' in '{}'", field.name, gdef.name)));
                                        }
                                        let (mut val, mut actual) = self.lower_scalar_body_expression(&field.value, slots)?;
                                        if let Some(cid) = match (&val, actual) {
                                            (Expr::ClosureLit(cid), _) => Some(*cid),
                                            (_, Type::Closure(cid)) => Some(cid),
                                            _ => None,
                                        } {
                                            has_closure = true;
                                            let (slit, env_info) = self.materialize_closure_struct_lit(cid, None, field.value.range.start)?;
                                            val = slit;
                                            actual = Type::Struct(env_info.env_sid);
                                        } else if matches!(actual, Type::Struct(env_sid) if self.generics.closure_envs.contains_key(&env_sid)) {
                                            has_closure = true;
                                        }
                                        if let Payload::Param(tp_idx) = gdef.fields[pos].1
                                            && inferred[tp_idx].is_none()
                                        {
                                            inferred[tp_idx] = Some(actual);
                                        }
                                        field_vals[pos] = Some((val, actual));
                                    }
                                    if !has_closure {
                                        return Err(self.error_at(target.range.start, &format!("'{name}' is a generic struct; write its type arguments, e.g. {name}[i64] {{ ... }}")));
                                    }
                                    let args: Vec<Type> = inferred.into_iter().collect::<Option<Vec<_>>>()
                                        .ok_or_else(|| self.error_at(target.range.start, &format!("cannot infer type arguments for generic struct '{name}'")))?;
                                    let sid = self.instantiate_struct(gid, args, target.range.start)?;
                                    let mut values: Vec<Option<Expr>> = vec![None; self.structs[sid].fields.len()];
                                    for (pos, entry) in field_vals.into_iter().enumerate() {
                                        if let Some((val, actual)) = entry {
                                            let expected = self.structs[sid].fields[pos].1;
                                            self.check_field_value_type(&self.structs[sid].fields[pos].0.clone(), expected, actual, None)?;
                                            if self.is_owned(actual)
                                                && let Expr::Variable(slot, ..) = &val
                                            {
                                                self.check_variable_move(*slot)?;
                                                self.moved.insert(*slot);
                                            }
                                            values[pos] = Some(val);
                                        }
                                    }
                                    return self.finish_struct_literal(sid, target.range.start, values);
                                }
                                None => return Err(self.error_at(target.range.start, &format!("unknown variable '{name}'")).with_code("E0101")),
                            }
                        }
                    }
                    // A generic instance (`Cell[T] { .. }`): the body was parsed
                    // with `T` substituted, so the registry already holds the
                    // concrete `StructDef` under the instance name.
                    ExpressionKind::Specialization { base, arguments } => {
                        let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                            return Err(self.error_at(base.range.start, "a generic struct literal needs a named target"));
                        };
                        if let Some(sid) = self.cached_struct_specialization(name, arguments) {
                            sid
                        } else {
                            // The typed route instantiates a generic struct
                            // literal on first sight; do the same here.
                            let &gid = self.generics.struct_names.get(name)
                                .ok_or_else(|| self.fail(&format!("'{name}' is not a generic struct")))?;
                            // A struct literal may name structs (even nested
                            // generic instances) as type arguments, which the
                            // strict call-site reader rejects by design.
                            let args = if let Some(shapes) = self.loose_shape_arguments(arguments) {
                                shapes.iter().map(|shape| self.shape_type(shape, target.range.start))
                                    .collect::<Option<Vec<_>>>()
                                    .ok_or_else(|| self.error_at(target.range.start, &format!("cannot read the type arguments of '{name}'")))?
                            } else {
                                let mut parsed = Vec::with_capacity(arguments.len());
                                for arg in arguments {
                                    self.seek_scalar_offset(arg.start);
                                    parsed.push(self.named_type()?);
                                }
                                parsed
                            };
                            self.instantiate_struct(gid, args, target.range.start)?
                        }
                    }
                    _ => return Err(self.error_at(target.range.start, "this construction form is not supported by the body route")),
                };
                if self.structs[sid].file != self.file && !self.structs[sid].public {
                    let (name, file) = (self.structs[sid].name.clone(), self.structs[sid].file.clone());
                    return Err(self.error_at(target.range.start, &format!("struct '{name}' is private to {file} (mark it 'pub' to export it)")));
                }
                let mut values: Vec<Option<Expr>> = (0..self.structs[sid].fields.len()).map(|_| None).collect();
                for field in fields {
                    let next = self.tokens.partition_point(|token| token.offset < field.name_range.end);
                    let (slot, expected) = self.constructor_field(sid, &field.name, &values, Some(self.tokens[next].offset))?;
                    let (value, actual) = self.lower_scalar_body_expression(&field.value, slots)?;
                    let (value, actual) = self.coerce_to_fn(value, actual, expected, &format!("field '{}'", field.name))?;
                    self.check_field_value_type(&field.name, expected, actual, Some(&field.value.range))?;
                    // An owned field value moves into the struct: the literal
                    // owns it, so the source slot's Frees is trimmed (the same
                    // rule the typed struct literal applies).
                    if self.is_owned(actual)
                        && let Expr::Variable(slot, ..) = &value {
                            self.seek_scalar_offset(field.value.range.end);
                            self.check_variable_move(*slot)?;
                            self.moved.insert(*slot);
                        }
                    values[slot] = Some(value);
                }
                self.finish_struct_literal(sid, target.range.start, values)?
            }
            ExpressionKind::Field { base, name } => {
                if let ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind {
                    // A struct parameter's scalar field read: the slot carries
                    // the struct type with its registry id, so the field type
                    // comes straight from the compiler's own table. A
                    // permissive scan can hand the lowering a chain the
                    // resolver never marked (a mutant, or a name the body
                    // never bound): that is a diagnostic, never a panic.
                    let Some(&def) = self.resolved_expressions().scalar_reads.get(&base.range.start)
                        .or_else(|| self.resolved_expressions().call_definitions.get(&base.range.start)) else {
                        return Err(self.error_at(expression.range.start, &format!("unknown variable '{base_name}'")).with_code("E0101"));
                    };
                    let Some(&(slot, struct_ty)) = slots.get(&def) else {
                        return Err(self.error_at(expression.range.start, &format!("unknown variable '{base_name}'")).with_code("E0101"));
                    };
                    let sid = match struct_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        other => return Err(self.error_at(expression.range.start, &format!("type {} has no fields or methods", other.name(self.ctx)))),
                    };
                    self.seek_scalar_offset(base.range.start);
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot use '{base_name}' because it was moved")).with_code("E0102"),
                            slot,
                        )));
                    }
                    let (line, column) = self.position();
                    let fty = self.field_ty_checked(sid, name)?;
                    (Expr::Field(Box::new(Expr::Variable(slot, struct_ty, line, column)), name.clone(), fty), fty)
                } else {
                    // A nested read (`o.inner.x`): the marked base lowers
                    // recursively and its declared type supplies the owner.
                    let (base_expr, base_ty) = self.lower_scalar_body_expression(base, slots)?;
                    let sid = match base_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        // A permissive scan can hand the lowering a chain whose
                        // base is not a struct; the typed route's diagnostics
                        // stay authoritative.
                        Type::Enum(enum_name, _) => {
                            self.seek_scalar_offset(expression.range.start);
                            return Err(self.fail(&format!("enum '{enum_name}' has no fields; bind its variant payloads with 'match'")));
                        }
                        other => {
                            self.seek_scalar_offset(expression.range.start);
                            return Err(self.fail(&format!("type {} has no fields or methods", other.name(self.ctx))));
                        }
                    };
                    let fty = self.field_ty_checked(sid, name)?;
                    (Expr::Field(Box::new(base_expr), name.clone(), fty), fty)
                }
            }
            ExpressionKind::Prefix { operator, operand } => {
                self.seek_scalar_offset(expression.range.start);
                let position = self.position();
                let operand = self.lower_scalar_body_expression(operand, slots)?;
                self.seek_scalar_offset(expression.range.end);
                let fact = self.resolved_expressions().scalar_expression_types.get(&expression.range).copied();
                self.lower_prefix(operator.clone(), operand, position, fact)?
            }
            ExpressionKind::Binary { left, operator, right } => {
                let lhs = self.lower_scalar_body_expression(left, slots)?;
                // A string `+` consumes its left operand before the right
                // side is even read: the typed route's postfix layer marks it
                // when the operator follows, so `s + s` fails on the second
                // read with E0102 (the source's move is already in effect).
                if *operator == Kind::Plus && matches!(lhs.1, Type::String | Type::StrView)
                    && let Expr::Variable(id, Type::String, ..) = &lhs.0 {
                        self.check_variable_move(*id)?;
                        self.moved.insert(*id);
                    }
                self.seek_scalar_offset(left.range.end);
                let position = self.position();
                let rhs = self.lower_scalar_body_expression(right, slots)?;
                self.seek_scalar_offset(expression.range.end);
                // The right operand is consumed by the same `+`; the left
                // one's move is already recorded above.
                let concat = *operator == Kind::Plus
                    && matches!(lhs.1, Type::String | Type::StrView)
                    && matches!(rhs.1, Type::String | Type::StrView);
                if concat
                    && let Expr::Variable(id, Type::String, ..) = &rhs.0 {
                        self.check_variable_move(*id)?;
                        self.moved.insert(*id);
                    }
                let fact = self.resolved_expressions().scalar_expression_types.get(&expression.range).copied();
                self.lower_infix(lhs, operator.clone(), rhs, position, fact)?
            }
            ExpressionKind::Call { callee, arguments } => {
                // A typed-only intrinsic (`ptr_write`, `ptr_drop_in_place`,
                // `bytes`): the typed expression parser owns the operational
                // checks and the shape. Bounded by the following operator.
                // The pointer-store intrinsics and `bytes` lower from the
                // shared nodes when their shapes are the supported ones; a
                // shape that needs a typed diagnostic keeps the delegation.
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                    && matches!(name.as_str(), "ptr_write" | "ptr_drop_in_place" | "bytes")
                    && let Some(lowered) = self.lower_shared_intrinsic(name, expression, arguments, slots)? {
                        return Ok(lowered);
                    }
                // `tokenize(text, sep)` builds a cursor over a string
                // variable (or a struct's string field): the base's borrow and
                // the separator check are the typed rule, applied to the
                // shared nodes. Any other shape keeps the typed delegation.
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                    && name == "tokenize"
                    && let Some(lowered) = self.lower_shared_tokenize(expression, arguments, slots)? {
                        return Ok(lowered);
                    }
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                    && typed_intrinsic(name) {
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.unsupported_expression()?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok(parsed);
                }
                // An enum variant with payload (`Shape::Circle(3)`): lower
                // the payload from the shared nodes. A qualified name that is
                // not a variant (an associated call) returns None and keeps
                // the typed delegation below.
                if self.resolved_expressions().enum_constructors.contains(&expression.range.start)
                    && let Some((value, ty)) = self.lower_shared_enum_construction(callee, arguments, slots)? {
                        return Ok((value, ty));
                    }
                // An associated call on a plain type (`Point::new(1)`,
                // `Shape::circle(2)`, `Self::circle(..)`) or a generic
                // instance (`Wrapper[i64]::new(..)`): the impl template
                // answers the signature and the shared argument lowering
                // builds the call. A receiver-taking entry, a private entry or
                // an unknown method returns None so the typed postfix layer
                // still owns those diagnostics.
                if let Some((value, ty)) = self.lower_shared_associated_call(callee, arguments, expression)? {
                    return Ok((value, ty));
                }
                // An associated call (`Point::new(1)`): the typed postfix layer
                // builds the instance and owns the pinned diagnostics. Bounded
                // by the following operator, like a chained method receiver.
                if let ExpressionKind::Qualified { name: method_name, .. } = &callee.kind {
                    if method_name == "drop" {
                        return Err(self.error_at(callee.range.start, "destructor 'drop' cannot be called directly"));
                    }
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.unsupported_expression()?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok(parsed);
                }
                if let ExpressionKind::Field { base, name: method_name } = &callee.kind {
                    if method_name == "drop" {
                        return Err(self.error_at(callee.range.start, "destructor 'drop' cannot be called directly"));
                    }
                    // A borrow argument (`pair.combine(&mut pair)`): an alias
                    // of the receiver slot with a mutable borrow in the same
                    // statement is the typed route's rejection.
                    if let ExpressionKind::Atom(Kind::Ident(receiver_name)) = &base.kind
                        && let Some(&(receiver_slot, _, receiver_ty)) = self.names.get(receiver_name) {
                        for argument in arguments {
                            if let ExpressionKind::BorrowArgument { name, name_range, mutable, .. } = &argument.kind
                                && self.names.get(name).is_some_and(|&(argument_slot, ..)| argument_slot == receiver_slot)
                                && (*mutable || matches!(receiver_ty, Type::StructMut(_))) {
                                    let message = if *mutable {
                                        format!("cannot borrow '{name}' mutably while it is borrowed in this statement")
                                    } else {
                                        format!("cannot borrow '{name}' while it is mutably borrowed in this statement")
                                    };
                                    return Err(self.error_at(name_range.start, &message));
                                }
                        }
                    }
                    if let Some(int_method) = IntMethodKind::from_name(method_name) {
                        let (receiver_expr, receiver_ty) = if let ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind {
                            let &(slot, _, ty) = self.names.get(receiver).ok_or_else(|| {
                                self.fail(&format!("unknown variable '{receiver}'")).with_code("E0101")
                            })?;
                            self.seek_scalar_offset(base.range.start);
                            let (line, column) = self.position();
                            (Expr::Variable(slot, ty, line, column), ty)
                        } else {
                            self.seek_scalar_offset(base.range.start);
                            self.lower_scalar_body_expression(base, slots)?
                        };
                        let Type::Int(width) = receiver_ty else {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail(&format!("type {} has no method '{method_name}'", receiver_ty.name(self.ctx))));
                        };
                        if int_method == IntMethodKind::CheckedNeg && !width.signed() {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail("checked_neg is only defined for signed integer types"));
                        }
                        let expected_arity = if int_method.is_unary() { 0 } else { 1 };
                        if arguments.len() != expected_arity {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail(&format!("'{method_name}' takes {expected_arity} argument(s), found {}", arguments.len())));
                        }
                        let arg_expr = if int_method.is_unary() {
                            None
                        } else {
                            let arg = &arguments[0];
                            self.seek_scalar_offset(arg.range.start);
                            let (arg_val, arg_ty) = self.lower_scalar_body_expression(arg, slots)?;
                            if arg_ty != receiver_ty {
                                self.seek_scalar_offset(arg.range.end);
                                return Err(self.fail(&format!(
                                    "argument to '{method_name}' must be {}, found {}",
                                    receiver_ty.name(self.ctx),
                                    arg_ty.name(self.ctx),
                                )));
                            }
                            Some(Box::new(arg_val))
                        };
                        self.seek_scalar_offset(expression.range.end);
                        let (line, column) = self.position();
                        let result_ty = if int_method.is_checked() {
                            let eid = self.instantiate_enum(0, vec![Type::Int(width)], callee.range.end)?;
                            Type::Enum(self.enums[eid].name, eid)
                        } else {
                            Type::Int(width)
                        };
                        return Ok((Expr::IntMethod(int_method, Box::new(receiver_expr), arg_expr, result_ty, line, column), result_ty));
                    }
                    // A borrow argument the shared rules cannot classify keeps
                    // the typed postfix layer's diagnostics; delegate the whole
                    // call, bounded by the following operator.
                    if arguments.iter().any(|argument| matches!(argument.kind, ExpressionKind::BorrowArgument { .. })) {
                        self.seek_scalar_offset(expression.range.start);
                        let parsed = self.unsupported_expression()?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok(parsed);
                    }
                    // A method on a temporary whose receiver is taken by
                    // value (`Counter::starting(1, 2).value()`): the
                    // associated call's return type resolves without lowering,
                    // so the chain is built here before any side effect. A
                    // borrowed receiver on a temporary, or any shape the
                    // precompute cannot answer, keeps the typed delegation
                    // below.
                    if !matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                        && let Some((value, ty)) = self.lower_shared_chain_call(expression, base, method_name, arguments, slots)? {
                            return Ok((value, ty));
                        }
                    // A deref through a chained receiver
                    // (`raw.deref().next.deref()`): the receiver lowers
                    // recursively and the pointer checks stay the typed
                    // route's own — the `unsafe` gate points at the method
                    // and a non-pointer receiver keeps the typed delegation
                    // below.
                    if !matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                        && method_name == "deref"
                        && arguments.is_empty()
                    {
                        let (operand, ty) = self.lower_scalar_body_expression(base, slots)?;
                        if let Type::Ptr(id) | Type::PtrMut(id) = ty {
                            let method = self.tokens.partition_point(|token| token.offset < callee.range.end) - 1;
                            let method_offset = self.tokens[method].offset;
                            self.check_pointer_deref(method_offset)?;
                            self.seek_scalar_offset(expression.range.end);
                            let (line, column) = self.position();
                            return Ok((Expr::DerefPtr(Box::new(operand), line, column), self.ctx.ptr_pointee(id)));
                        }
                    }
                    // A method on a temporary or chained result
                    // (`start.shifted(1, 2).shifted(0, -6)`): the typed postfix
                    // layer owns the receiver semantics and the diagnostics;
                    // delegate the whole call. The binding power comes from the
                    // operator that follows this operand in the shared tree, so
                    // the typed parse stops at the operand's end and the
                    // enclosing operator (`... .value() == 11`) stays with the
                    // shared lowering.
                    if !matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))) {
                        self.seek_scalar_offset(expression.range.start);
                        let parsed = self.unsupported_expression()?;
                        // The typed parse consumed exactly this operand; the
                        // surrounding shared nodes own what follows.
                        self.seek_scalar_offset(expression.range.end);
                        return Ok(parsed);
                    }
                    // A method receiver is a borrow, not a value read: its
                    // slot comes from `names`. The re-check admitted only
                    // calls `resolve_inherent_method` answers, so this path
                    // and the gate agree.
                    if let ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind
                        && let Some(&(slot, _, ty)) = self.names.get(receiver)
                            && !matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                                if let Some((signature, _, _)) = self.resolve_inherent_method(method_name, ty) {
                                    return self.lower_method_call(method_name, slot, ty, signature, expression, arguments, slots);
                                }
                                if let Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) = ty
                                    && let Some((_, fty)) = self.structs[sid].fields.iter().find(|(f, _)| f == method_name).cloned()
                                {
                                    if matches!(fty, Type::Fn(_)) {
                                        let (callee_expr, callee_ty) = self.lower_scalar_body_expression(callee, slots)?;
                                        if let Type::Fn(sig) = callee_ty {
                                            let (args, ret) = self.lower_fn_sig_call_args(method_name, sig, expression, arguments, slots)?;
                                            self.seek_scalar_offset(expression.range.end);
                                            return Ok((Expr::FnExprCall(Box::new(callee_expr), args, ret, sig), ret));
                                        }
                                    } else if let Type::Struct(env_sid) = fty
                                        && let Some(env) = self.generics.closure_envs.get(&env_sid).cloned()
                                    {
                                        let (callee_expr, _) = self.lower_scalar_body_expression(callee, slots)?;
                                        let (mut args, ret) = self.lower_fn_sig_call_args(method_name, env.sig, expression, arguments, slots)?;
                                        self.seek_scalar_offset(callee.range.start);
                                        let (line, column) = self.position();
                                        let env_arg = match env.kind {
                                            CallableKind::Fn => Argument::Value(Expr::BorrowAs(Box::new(callee_expr), false, line, column)),
                                            CallableKind::FnMut => Argument::Value(Expr::BorrowAs(Box::new(callee_expr), true, line, column)),
                                            CallableKind::FnOnce => Argument::Value(callee_expr),
                                        };
                                        args.insert(0, env_arg);
                                        self.seek_scalar_offset(expression.range.end);
                                        return Ok((Expr::Call(env.wrapper_fn_id, args, ret), ret));
                                    }
                                }
                                // A generic-instance receiver resolves lazily
                                // here (the gate is permissive and stateless);
                                // a missing method reproduces the typed route's
                                // pinned diagnostics.
                                let sid = match ty {
                                    Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => Some((sid, false)),
                                    Type::Enum(_, sid) => Some((sid, true)),
                                    _ => None,
                                };
                                if let Some((sid, is_enum)) = sid {
                                    let origin = if is_enum {
                                        self.generics.enum_origin.get(&sid).cloned()
                                    } else {
                                        self.generics.struct_origin.get(&sid).cloned()
                                    };
                                    if let Some((gid, origin_args)) = origin {
                                        let template = if is_enum {
                                            self.generics.defs[gid].name.clone()
                                        } else {
                                            self.generics.struct_defs[gid].name.clone()
                                        };
                                        let Some(gimpl) = self.generics.generic_impls.get(&template).cloned() else {
                                            return Err(self.error_at(callee.range.end, &format!("the impl for '{template}' has no method '{method_name}'")));
                                        };
                                        let mut names: Vec<String> = gimpl.methods.iter().map(|m| m.name.clone()).collect();
                                        names.sort();
                                        let Some(mdef) = gimpl.methods.iter().find(|m| m.name == *method_name) else {
                                            return Err(self.error_at(callee.range.end, &format!("the impl for '{template}' has no method '{method_name}'; it has {}", names.join(", "))));
                                        };
                                        if mdef.receiver.is_some() {
                                            let (id, params, ret) = self.instantiate_impl_method(&template, method_name, sid, gid, &origin_args, expression.range.start)?;
                                            let signature = Signature { id, params, ret, file: self.file.clone(), public: true, extern_c: false };
                                            return self.lower_method_call(method_name, slot, ty, signature, expression, arguments, slots);
                                        }
                                        return Err(self.error_at(callee.range.end, &format!("'{template}::{method_name}' is an associated function, so it takes no receiver")));
                                    }
                                    let type_name = match ty {
                                        Type::Enum(name, _) => name.to_owned(),
                                        _ => self.structs[sid].name.clone(),
                                    };
                                    // The table knows the method but its entry
                                    // declares no receiver: a value call is the
                                    // typed route's associated-function error,
                                    // not a missing method.
                                    if self.generics.inherent.get(&format!("{type_name}#{method_name}"))
                                        .is_some_and(|definition| definition.receiver.is_none())
                                    {
                                        return Err(self.error_at(callee.range.end, &format!("'{type_name}::{method_name}' is an associated function, so it takes no receiver; call it as '{type_name}::{method_name}(...)'")));
                                    }
                                    let mut names: Vec<String> = self.generics.inherent.keys()
                                        .filter(|key| key.starts_with(&format!("{type_name}#")))
                                        .filter_map(|key| key.split('#').nth(1).map(|name| name.to_owned()))
                                        .collect();
                                    names.sort();
                                    if names.is_empty() {
                                        return Err(self.error_at(callee.range.end, &format!("type '{type_name}' has no methods; declare one in 'impl {type_name} {{ ... }}'")));
                                    }
                                    return Err(self.error_at(callee.range.end, &format!("type '{type_name}' has no method '{method_name}'; it has {}", names.join(", "))));
                                }
                                return Err(self.fail(&format!("type {} has no fields or methods", ty.name(self.ctx))));
                            }
                    // The receiver's slot comes from `names`: a pointer binding
                    // the resolver could not spell yet (the result of
                    // `buf.offset(1)`, say) still has an authoritative slot by
                    // the time this statement lowers.
                    let (operand, ty) = if let ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind {
                        let &(slot, _, ty) = self.names.get(receiver).ok_or_else(|| {
                            self.fail(&format!("unknown variable '{receiver}'")).with_code("E0101")
                        })?;
                        self.seek_scalar_offset(base.range.start);
                        let (line, column) = self.position();
                        (Expr::Variable(slot, ty, line, column), ty)
                    } else {
                        self.lower_scalar_body_expression(base, slots)?
                    };
                    let (Type::Ptr(id) | Type::PtrMut(id)) = ty else {
                        return Err(self.fail(&format!("type {} has no fields or methods", ty.name(self.ctx))));
                    };
                    let method = self.tokens.partition_point(|token| token.offset < callee.range.end) - 1;
                    let method_offset = self.tokens[method].offset;
                    // `offset(n)` / `byte_offset(n)` (ADR-0209 step A): the
                    // address moves by n elements or n bytes, the result is
                    // the same pointer type. The gate's marks admitted only
                    // these three names, so anything else is `deref`'s
                    // diagnostic below.
                    if method_name == "offset" || method_name == "byte_offset" {
                        if self.unsafe_depth == 0 {
                            return Err(self.error_at(method_offset, "pointer arithmetic requires 'unsafe'"));
                        }
                        let [count] = arguments.as_slice() else {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail(&format!("{method_name} takes an i64 count")));
                        };
                        let (count_value, count_ty) = self.lower_scalar_body_expression(count, slots)?;
                        if count_ty != Type::Int(IntTy::I64) {
                            self.seek_scalar_offset(count.range.end);
                            return Err(self.fail(&format!("{method_name} takes an i64 count")));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        let (line, column) = self.position();
                        let bytewise = method_name == "byte_offset";
                        return Ok((Expr::PtrOffset(Box::new(operand), Box::new(count_value), bytewise, line, column), ty));
                    }
                    // `is_null` is safe (spec/unsafe.md 2.1): it reads
                    // nothing, so no `unsafe` gate. `deref` stays gated.
                    if method_name == "is_null" {
                        if !arguments.is_empty() {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail("is_null takes no arguments"));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::PtrIsNull(Box::new(operand)), Type::Bool));
                    }
                    if method_name != "deref" {
                        // The typed route consumes the name and fails on the
                        // token that follows it (the `(`), so the span lands
                        // there.
                        self.seek_scalar_offset(callee.range.end);
                        return Err(self.fail(&format!("pointers have no method '{method_name}'")));
                    }
                    self.check_pointer_deref(method_offset)?;
                    self.seek_scalar_offset(expression.range.end);
                    let (line, column) = self.position();
                    return Ok((Expr::DerefPtr(Box::new(operand), line, column), self.ctx.ptr_pointee(id)));
                }
                if let ExpressionKind::BracketedCallTarget { base, arguments: type_arguments } = &callee.kind {
                    // `ptr::null[T]()` (spec/unsafe.md 2.1): a qualified path
                    // with one type argument naming the pointee. The value is
                    // a null `*mut T`; `is_null` is the only read.
                    if let ExpressionKind::Qualified { base: prefix, name } = &base.kind
                        && matches!(prefix.kind, ExpressionKind::Atom(Kind::Ident(ref word)) if word == "ptr")
                        && name == "null"
                        && type_arguments.len() == 1
                    {
                        let spelling = self.source.get(type_arguments[0].clone())
                            .ok_or_else(|| self.error_at(type_arguments[0].start, "cannot read the null pointer's pointee type"))?
                            .trim().to_owned();
                        let pointee_ty = self.subst.get(spelling.as_str()).copied()
                            .or_else(|| super::types::scalar_type(&spelling))
                            .or_else(|| self.struct_names.get(&spelling).map(|&sid| Type::Struct(sid)))
                            .or_else(|| self.enum_names.get(&spelling).map(|&eid| Type::Enum(self.enums[eid].name.clone(), eid)))
                            .ok_or_else(|| self.error_at(type_arguments[0].start, &format!("'ptr::null' needs a pointee type; '{spelling}' is not one")))?;
                        let id = self.ctx.intern_pointee(pointee_ty);
                        self.seek_scalar_offset(expression.range.end);
                        let (line, column) = self.position();
                        return Ok((Expr::PtrNull(id, line, column), Type::PtrMut(id)));
                    }
                    let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                        return Err(self.fail("this call form is not supported by the body route"));
                    };
                    // The C layout queries are spelled like a generic call but read a
                    // repr(C) layout: cursor on the `[` for the shared reader.
                    if matches!(name.as_str(), "c_size_of" | "c_align_of" | "c_offset_of") {
                        self.seek_scalar_offset(base.range.start);
                        self.pos += 1;
                        let (value, ty) = self.c_layout_expr(name, arguments.first())?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((value, ty));
                    }
                    let Some(&gid) = self.generics.fn_names.get(name) else {
                        return Err(self.error_at(base.range.start, &format!("'{name}' is not a generic function")));
                    };
                    // An explicit generic call: the type arguments choose the
                    // instance. Instantiate first (the same instance map the
                    // typed route fills), then lower the argument list against
                    // the substituted signature.
                    self.seek_scalar_offset(base.range.start);
                    // `seek_scalar_offset` lands on the name; the type
                    // argument reader starts at the `[` that follows.
                    self.pos += 1;
                    let type_args = self.generic_call_type_arguments(base.range.start)?;
                    let (id, params, ret) = self.instantiate_fn(gid, type_args, base.range.start)?;
                    // The argument list follows the type arguments; lower it
                    // against the substituted signature with the same value,
                    // borrow and move rules a direct call uses.
                    let args = self.lower_value_arguments(name, arguments, &params, slots)?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok((Expr::Call(id, args, ret), ret));
                }
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind {
                    if resolved_builtin(name) {
                        return self.lower_resolved_builtin(name, expression, callee, arguments, slots);
                    }
                    // A call on a closure value: the typed machinery reads the
                    // captured slots at the call site (the leading arguments)
                    // and parses the visible argument list.
                    if matches!(self.names.get(name), Some(&(_, _, Type::Closure(_)))) {
                        self.seek_scalar_offset(callee.range.end);
                        let (value, ty) = self.closure_call_with(name, callee.range.start, arguments, slots)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((value, ty));
                    }
                    if let Some(&(slot, mutable, ty)) = self.names.get(name)
                        && let Type::Struct(env_sid) | Type::StructRef(env_sid) | Type::StructMut(env_sid) = ty
                        && let Some(env) = self.generics.closure_envs.get(&env_sid).cloned()
                    {
                        self.seek_scalar_offset(callee.range.start);
                        if self.moved.contains(&slot) {
                            return Err(moved_value_hint(self.binding_label(
                                self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"),
                                slot,
                            )));
                        }
                        let (line, column) = self.position();
                        let (mut args, ret) = self.lower_fn_sig_call_args(name, env.sig, expression, arguments, slots)?;
                        let consume_once = env.kind == CallableKind::FnOnce || self.fn_once_slots.contains(&slot);
                        if consume_once {
                            self.moved.insert(slot);
                        }
                        let env_arg = match env.kind {
                            CallableKind::Fn => Argument::Borrow(slot, matches!(ty, Type::Struct(_))),
                            CallableKind::FnMut => {
                                if !mutable && matches!(ty, Type::Struct(_)) {
                                    return Err(self.error_at(callee.range.start, &format!("cannot call FnMut closure '{name}' because it is not declared 'var'")));
                                }
                                Argument::Borrow(slot, matches!(ty, Type::Struct(_)))
                            }
                            CallableKind::FnOnce => {
                                self.check_variable_move(slot)?;
                                Argument::Value(Expr::Variable(slot, ty, line, column))
                            }
                        };
                        args.insert(0, env_arg);
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::Call(env.wrapper_fn_id, args, ret), ret));
                    }
                    // A call through a function-typed value: the binding's
                    // slot names the pointer the caller passed (ADR-0189).
                    if let Some(&(slot, _, Type::Fn(sig))) = self.names.get(name) {
                        if self.moved.contains(&slot) {
                            return Err(moved_value_hint(self.binding_label(
                                self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"),
                                slot,
                            )));
                        }
                        if self.fn_once_slots.contains(&slot) {
                            self.moved.insert(slot);
                        }
                        let (params, ret) = self.ctx.fn_sig(sig);
                        // Borrow arguments forwarded through a function value:
                        // the shared argument lowering already applies the
                        // classify rules and the statement-scoped borrow
                        // registers, so a full-arity call lowers here. A shape
                        // it does not cover keeps the typed delegation below.
                        if arguments.len() == params.len()
                            && arguments.iter().any(|argument| matches!(argument.kind, ExpressionKind::BorrowArgument { .. }))
                        {
                            let mut nodes_left = 128;
                            let mut effects = SharedExpressionEffects::default();
                            if let Some(args) = self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects) {
                                self.commit_shared_effects(effects);
                                self.seek_scalar_offset(expression.range.end);
                                return Ok((Expr::FnCall(slot, args, ret, sig), ret));
                            }
                            // The helper refused a shape it cannot answer; a
                            // same-slot borrow in one statement is the typed
                            // route's rejection, so report it here.
                            let mut borrows: Vec<(usize, String, usize, bool)> = Vec::new();
                            for argument in arguments {
                                if let ExpressionKind::BorrowArgument { name, name_range, mutable, .. } = &argument.kind
                                    && let Some(&(argument_slot, ..)) = self.names.get(name) {
                                        if let Some((_, earlier_name, earlier_at, earlier_mutable)) = borrows.iter().find(|(slot, ..)| *slot == argument_slot) {
                                            let (subject, at) = if *earlier_name == *name {
                                                (name.clone(), name_range.start)
                                            } else {
                                                (earlier_name.clone(), *earlier_at)
                                            };
                                            let message = if *mutable || *earlier_mutable {
                                                format!("cannot borrow '{subject}' mutably while it is borrowed in this statement")
                                            } else {
                                                format!("cannot borrow '{subject}' while it is mutably borrowed in this statement")
                                            };
                                            return Err(self.error_at(at, &message));
                                        }
                                        borrows.push((argument_slot, name.clone(), name_range.start, *mutable));
                                    }
                            }
                        }
                        // A borrow argument the shared lowering refused can
                        // still be a typed diagnostic (an alias of the same
                        // slot in one statement); delegate the whole call,
                        // bounded by the following operator.
                        if arguments.iter().any(|argument| matches!(argument.kind, ExpressionKind::BorrowArgument { .. })) {
                            self.seek_scalar_offset(expression.range.start);
                            let parsed = self.unsupported_expression()?;
                            self.seek_scalar_offset(expression.range.end);
                            return Ok(parsed);
                        }
                        let (args, ret) = self.lower_fn_sig_call_args(name, sig, expression, arguments, slots)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::FnCall(slot, args, ret, sig), ret));
                    }
                }
                let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else {
                    let (callee_expr, callee_ty) = self.lower_scalar_body_expression(callee, slots)?;
                    if let Type::Fn(sig) = callee_ty {
                        let (args, ret) = self.lower_fn_sig_call_args("<fn>", sig, expression, arguments, slots)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::FnExprCall(Box::new(callee_expr), args, ret, sig), ret));
                    }
                    return Err(self.error_at(callee.range.start, &format!("cannot call a value of type {}", callee_ty.name(self.ctx))));
                };
                let signature = match self.functions.get(name).cloned() {
                    Some(signature) => signature,
                    // A bare call infers direct value parameters (and the
                    // existing Callable-bound shapes). Without enough
                    // evidence, keep the explicit-type-argument remedy.
                    None if self.generics.fn_names.contains_key(name) => {
                        let gid = self.generics.fn_names[name];
                        if let Some((call_expr, call_ret)) = self.try_lower_inferred_generic_call(name, gid, callee.range.start, arguments, slots)? {
                            self.seek_scalar_offset(expression.range.end);
                            return Ok((call_expr, call_ret));
                        }
                        return Err(self.error_at(callee.range.start, &format!("'{name}' is a generic function; write its type arguments, e.g. {name}[i64](...)")));
                    }
                    // Not a plain function: a trait method of one of this
                    // instance's constrained parameters. The substitution picks
                    // the concrete impl, so the call stays monomorphized.
                    None => self.resolve_trait_call(name, callee.range.start)?,
                };
                if signature.file != self.file && !signature.public {
                    // The typed route's import visibility rule.
                    return Err(self.error_at(callee.range.start, &format!("function '{name}' is private to {} (mark it 'pub' to export it)", signature.file)));
                }
                if (signature.extern_c || self.ctx.is_unsafe_fn(signature.id)) && self.unsafe_depth == 0 {
                    return Err(self.error_at(callee.range.start, &format!("call to unsafe function '{name}' requires 'unsafe'")).with_code("E0701"));
                }
                                let args = self.lower_value_arguments(name, arguments, &signature.params, slots)?;
                self.seek_scalar_offset(expression.range.end);
                (Expr::Call(signature.id, args, signature.ret), signature.ret)
            }
            ExpressionKind::Qualified { .. } => {
                // A payload-free variant path (`Shape::Empty`) lowers from the
                // shared node; any other qualified value keeps the typed
                // parser (an associated function used as a value).
                if let Some((value, ty)) = self.lower_shared_enum_construction(expression, &[], slots)? {
                    return Ok((value, ty));
                }
                self.seek_scalar_offset(expression.range.start);
                let parsed = self.unsupported_expression()?;
                self.seek_scalar_offset(expression.range.end);
                return Ok(parsed);
            }
            ExpressionKind::Specialization { base, arguments } => {
                // A bracket the syntax tree kept as a specialization on a
                // value name (`arr[j]`): the bracket contents are a spelling
                // range, so the resolved scan has no read facts for the index.
                // A name binding indexed by a bare name or number is lowered
                // here by consulting the binding table; every other shape
                // keeps the typed disambiguation.
                let mut lowered = None;
                if arguments.len() == 1
                    && let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind
                    && !self.struct_names.contains_key(name)
                    && !self.enum_names.contains_key(name)
                    && !self.generics.struct_names.contains_key(name)
                    && !self.generics.names.contains_key(name)
                    && let Some(&(slot, _, ty)) = self.names.get(name)
                    && matches!(ty, Type::ArrayOf(..) | Type::Vector(_) | Type::VectorRef(_))
                {
                    if self.moved.contains(&slot) {
                        self.seek_scalar_offset(base.range.start);
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"), slot,
                        )));
                    }
                    let start = self.tokens.partition_point(|token| token.offset < arguments[0].start);
                    if let Ok((index, _)) = ae_syntax::expression::parse_expression_tokens(&self.file, self.source, &self.tokens, start, false) {
                        self.seek_scalar_offset(index.range.start);
                        let index_position = self.position();
                        let index_value = match &index.kind {
                            ExpressionKind::Atom(Kind::Ident(bound)) => self.names.get(bound).map(|&(id, _, ity)| (Expr::Variable(id, ity, index_position.0, index_position.1), ity)),
                            ExpressionKind::Atom(Kind::Number(value)) => Some((Expr::Number(*value), Type::Int(IntTy::I64))),
                            _ => None,
                        };
                        if let Some((index_value, Type::Int(IntTy::I64))) = index_value {
                            self.seek_scalar_offset(base.range.end);
                            let (line, column) = self.position();
                            lowered = Some(match ty {
                                Type::ArrayOf(elem_id, len) => {
                                    let elem_ty = self.ctx.array_elem(elem_id);
                                    (Expr::Index(slot, len, Box::new(index_value), line, column, elem_ty), elem_ty)
                                }
                                Type::Slice(id) | Type::SliceMut(id) => {
                                    let element = self.ctx.slice_elem(id);
                                    (Expr::SliceIndex(slot, Box::new(index_value), line, column, element), vector_element_read_type(element, self.structs))
                                }
                                Type::Vector(id) | Type::VectorRef(id) => {
                                    let element = self.ctx.vector_elem(id);
                                    let borrowed = matches!(ty, Type::VectorRef(_));
                                    if element == Type::String {
                                        (Expr::StrVecIndex(slot, Box::new(index_value), line, column, borrowed), Type::StrView)
                                    } else if let Type::Vector(inner) = element {
                                        // An owned element is a view: the
                                        // outer vector keeps the storage.
                                        (Expr::VecIndex(slot, Box::new(index_value), line, column, borrowed, element), Type::VectorRef(inner))
                                    } else if let Type::Struct(sid) = element
                                        && !self.structs[sid].is_copy {
                                        // An owned struct element reads as a
                                        // borrow; a Copy struct stays a value.
                                        (Expr::VecIndex(slot, Box::new(index_value), line, column, borrowed, element), Type::StructRef(sid))
                                    } else {
                                        (Expr::VecIndex(slot, Box::new(index_value), line, column, borrowed, element), element)
                                    }
                                }
                                _ => unreachable!(),
                            });
                        }
                    }
                }
                match lowered {
                    Some((value, ty)) => (value, ty),
                    None => {
                        self.seek_scalar_offset(expression.range.start);
                        self.unsupported_expression()?
                    }
                }
            }
            ExpressionKind::Index { base, index } => {
                // A receiver that is not a bare binding (`r.cells[0]`): the
                // field expression is lowered as an lvalue and the element
                // read goes through it.
                if !matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))) {
                    let (base_expr, base_ty) = self.lower_scalar_body_expression(base, slots)?;
                    self.seek_scalar_offset(base.range.end);
                    let (line, column) = self.position();
                    let id = match base_ty {
                        Type::Vector(id) | Type::VectorRef(id) => id,
                        other => return Err(self.error_at(base.range.start, &format!("cannot index a value of type {}", other.name(self.ctx)))),
                    };
                    let element = self.ctx.vector_elem(id);
                    let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                    if index_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(index.range.end);
                        return Err(self.fail("vector index must be an integer"));
                    }
                    let read_ty = vector_element_read_type(element, self.structs);
                    return Ok((Expr::VecIndexOf(Box::new(base_expr), Box::new(value), line, column, element), read_ty));
                }
                let index_name = match &base.kind {
                    ExpressionKind::Atom(Kind::Ident(name)) => name.as_str(),
                    _ => "value",
                };
                let (slot, ty) = self.resolved_slot(
                    self.resolved_expressions().scalar_reads.get(&base.range.start),
                    slots, base.range.start, index_name)?;
                self.seek_scalar_offset(base.range.end);
                let (line, column) = self.position();
                match ty {
                    Type::ArrayOf(elem_id, len) => {
                        let elem_ty = self.ctx.array_elem(elem_id);
                        let value = self.lower_scalar_array_index(index, slots)?;
                        (Expr::Index(slot, len, Box::new(value), line, column, elem_ty), elem_ty)
                    }
                    Type::Slice(id) | Type::SliceMut(id) => {
                        let element = self.ctx.slice_elem(id);
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int(IntTy::I64) {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let read_ty = vector_element_read_type(element, self.structs);
                        (Expr::SliceIndex(slot, Box::new(value), line, column, element), read_ty)
                    }
                    Type::Vector(id) | Type::VectorRef(id) => {
                        let element = self.ctx.vector_elem(id);
                        let borrowed = matches!(ty, Type::VectorRef(_));
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int(IntTy::I64) {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let read_ty = vector_element_read_type(element, self.structs);
                        if element == Type::String {
                            // A borrowed string vector's element is a view; an
                            // owned one hands the element's view too.
                            (Expr::StrVecIndex(slot, Box::new(value), line, column, borrowed), read_ty)
                        } else {
                            (Expr::VecIndex(slot, Box::new(value), line, column, borrowed, element), read_ty)
                        }
                    }
                    other => return Err(self.error_at(base.range.start, &format!("cannot index a value of type {}", other.name(self.ctx)))),
                }
            }
            ExpressionKind::Vector(elements) => {
                // A vector literal used as a value (`a = vec[9];`): the same
                // construction the Declare arm builds.
                self.seek_scalar_offset(expression.range.start);
                let (line, column) = self.position();
                let mut values = Vec::with_capacity(elements.len());
                let mut element_type = None;
                for (index, element) in elements.iter().enumerate() {
                    if index >= 65536 {
                        self.seek_scalar_offset(element.range.start);
                        return Err(self.fail("vector element limit exceeded (65536)"));
                    }
                    let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                    self.seek_scalar_offset(element.range.end);
                    if self.is_owned(ty)
                        && let Expr::Variable(source, ..) = &value {
                            self.check_variable_move(*source)?;
                            self.moved.insert(*source);
                        }
                    element_type = Some(self.check_numeric_vector_element(element_type, ty)?);
                    values.push(value);
                }
                let elem = element_type.unwrap_or(Type::Int(IntTy::I64));
                let ty = Type::Vector(self.ctx.intern_vector_elem(elem));
                (Expr::VecLit(values, ty, line, column), ty)
            }
            _ => return Err(self.error_at(expression.range.start, "this expression is not supported by the body route")),
        })
    }


    /// A method call on a temporary whose receiver is a by-value `self`
    /// (`Counter::starting(1, 2).value()`): the receiver's type comes from the
    /// associated call's declared return, the method resolves from the
    /// inherent impl table, and the receiver is passed as `Argument::Value`
    /// exactly like the typed postfix layer moves a temporary into a by-value
    /// method. `None` means the shape is not one of these (a borrowed
    /// receiver, an unknown method, another root): the caller keeps the typed
    /// delegation, and no lowering side effect has happened.
    pub(super) fn lower_shared_chain_call(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        base: &ae_syntax::expression::Expression,
        method: &str,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        let Some(receiver_ty) = self.chain_return_type(base) else {
            return Ok(None);
        };
        let Some((signature, receiver, _method_receiver_ty)) = self.resolve_inherent_method(method, receiver_ty) else {
            return Ok(None);
        };
        // A borrowed receiver needs a bare named variable as storage. The
        // typed route consumes the argument list first and then reports the
        // rule at the call's closing `)`, so do the same.
        if matches!(receiver, Some(Type::StructRef(_) | Type::StructMut(_)))
            || matches!(receiver_ty, Type::StructRef(_) | Type::StructMut(_))
        {
            let params = signature.params[1..].to_vec();
            let mut nodes_left = 128;
            let mut effects = SharedExpressionEffects::default();
            if self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects).is_none() {
                return Ok(None);
            }
            self.seek_scalar_offset(expression.range.end);
            return Err(self.fail(&format!("'{method}' borrows its receiver, so the receiver must be a bare struct variable; bind the value to a variable first")));
        }
        if !matches!(receiver, Some(ty) if ty == receiver_ty) {
            return Ok(None);
        }
        let ret = signature.ret;
        let params = signature.params[1..].to_vec();
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        let Some(rest) = self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects) else {
            return Ok(None);
        };
        let (receiver_value, actual) = self.lower_scalar_body_expression(base, slots)?;
        if actual != receiver_ty {
            return Ok(None);
        }
        self.commit_shared_effects(effects);
        let mut args = Vec::with_capacity(rest.len() + 1);
        args.push(Argument::Value(receiver_value));
        args.extend(rest);
        self.seek_scalar_offset(expression.range.end);
        Ok(Some((Expr::Call(signature.id, args, ret), ret)))
    }

    /// The declared result type of a call chain, without lowering it:
    /// `Type::method(...)` on a plain struct or enum answers from the inherent
    /// impl entry when the entry is receiver-less and the arity matches; a
    /// by-value method on such a chain descends recursively. Only shapes whose
    /// evaluation the chain helper can then repeat are admitted.
    fn chain_return_type(&self, base: &ae_syntax::expression::Expression) -> Option<Type> {
        use ae_syntax::expression::ExpressionKind;
        match &base.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => self.names.get(name).map(|&(_, _, ty)| ty),
            ExpressionKind::Call { callee, arguments } => match &callee.kind {
                ExpressionKind::Qualified { base: target, name: method } => {
                    let type_name = match &target.kind {
                        ExpressionKind::Atom(Kind::Ident(name)) => {
                            if name == "Self" {
                                match self.impl_self {
                                    Some(Type::Struct(sid)) => self.structs[sid].name.clone(),
                                    Some(Type::Enum(name, _)) => name.to_owned(),
                                    _ => return None,
                                }
                            } else {
                                name.clone()
                            }
                        }
                        _ => return None,
                    };
                    let key = format!("{type_name}#{method}");
                    let def = self.generics.inherent.get(&key)?;
                    if def.receiver.is_some() || arguments.len() != def.signature.params.len() {
                        return None;
                    }
                    Some(def.signature.ret)
                }
                ExpressionKind::Field { base: inner, name: method } => {
                    let inner_ty = self.chain_return_type(inner)?;
                    let (signature, receiver, _) = self.resolve_inherent_method(method, inner_ty)?;
                    if !matches!(receiver, Some(ty) if ty == inner_ty) {
                        return None;
                    }
                    if arguments.len() + 1 != signature.params.len() {
                        return None;
                    }
                    Some(signature.ret)
                }
                // A direct call root (`make().area()`): the function's
                // declared return types the chain.
                ExpressionKind::Atom(Kind::Ident(name)) => {
                    let signature = self.functions.get(name)?;
                    (arguments.len() == signature.params.len()).then_some(signature.ret)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `ptr_write`, `ptr_drop_in_place` and `bytes` from the shared nodes.
    /// Only successful shapes lower here; an unsafe-context violation, a
    /// wrong pointer or a wrong argument type returns `Ok(None)` so the typed
    /// parser still owns those pinned diagnostics.
    pub(super) fn lower_shared_intrinsic(
        &mut self,
        name: &str,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        match name {
            "ptr_write" => {
                if arguments.len() != 2 || self.unsafe_depth == 0 {
                    return Ok(None);
                }
                let (pointer, pointer_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                let Type::PtrMut(pointee_id) = pointer_ty else {
                    return Ok(None);
                };
                let pointee = self.ctx.ptr_pointee(pointee_id);
                let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                if value_ty != pointee {
                    return Ok(None);
                }
                if self.is_owned(value_ty)
                    && let Expr::Variable(slot, ..) = &value {
                        self.check_variable_move(*slot)?;
                        self.moved.insert(*slot);
                    }
                self.seek_scalar_offset(expression.range.end);
                let (line, column) = self.position();
                Ok(Some((Expr::PtrWrite(Box::new(pointer), Box::new(value), pointee, line, column), Type::Int(IntTy::I64))))
            }
            "ptr_drop_in_place" => {
                if arguments.len() != 1 || self.unsafe_depth == 0 {
                    return Ok(None);
                }
                let (pointer, pointer_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                let Type::PtrMut(pointee_id) = pointer_ty else {
                    return Ok(None);
                };
                let pointee = self.ctx.ptr_pointee(pointee_id);
                self.seek_scalar_offset(expression.range.end);
                let (line, column) = self.position();
                Ok(Some((Expr::PtrDropInPlace(Box::new(pointer), pointee, line, column), Type::Int(IntTy::I64))))
            }
            "bytes" => {
                if arguments.len() != 1 {
                    return Ok(None);
                }
                let (value, ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                if !matches!(ty, Type::String | Type::StrView) {
                    return Ok(None);
                }
                self.seek_scalar_offset(expression.range.end);
                let (line, column) = self.position();
                let ty = Type::Vector(self.ctx.intern_vector_elem(Type::Int(IntTy::I64)));
                Ok(Some((Expr::Bytes(Box::new(value), line, column), ty)))
            }
            _ => Ok(None),
        }
    }

    /// Lower one bound of a `&name[start..end]` slice argument (ADR-0236
    /// rung 3): the bracket range is parsed back into a scalar expression,
    /// which must be i64.
    fn lower_slice_bound(
        &mut self,
        range: std::ops::Range<usize>,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let start = self.tokens.partition_point(|token| token.offset < range.start);
        let Ok((expression, _)) = ae_syntax::expression::parse_expression_tokens(&self.file, self.source, &self.tokens, start, false) else {
            return Err(self.error_at(range.start, "invalid slice range bound"));
        };
        self.seek_scalar_offset(range.start);
        let (value, ty) = self.lower_scalar_body_expression(&expression, slots)?;
        if ty != Type::Int(IntTy::I64) {
            return Err(self.fail("slice range bounds must be integers"));
        }
        Ok(value)
    }

    /// The value/borrow/view argument rules for a direct or generic call:
    /// function-typed parameters take a function value, borrow parameters take
    /// a borrow argument with the statement's alias checks, `&string` takes a
    /// view, and an owned value moves. The typed reader used to own this for
    /// generic calls; both call forms share it now.
    pub(super) fn lower_value_arguments(
        &mut self,
        callee_name: &str,
        arguments: &[ae_syntax::expression::Expression],
        params: &[Type],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Vec<Argument>, ae_span::Diagnostic> {
        self.lower_value_arguments_with(callee_name, arguments, params, slots, Vec::new())
    }

    fn lower_value_arguments_with(
        &mut self,
        callee_name: &str,
        arguments: &[ae_syntax::expression::Expression],
        params: &[Type],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
        mut prelowered: Vec<Option<(Expr, Type)>>,
    ) -> Result<Vec<Argument>, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let mut args = Vec::with_capacity(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            self.seek_scalar_offset(argument.range.start);
            if index >= 64 {
                return Err(self.fail("function argument limit exceeded (64)"));
            }
            let &expected = params.get(index)
                .ok_or_else(|| self.fail(&format!("too many arguments to '{callee_name}'")))?;
            // A function-typed parameter takes a closure literal (or
            // a named function) as `Argument::FnPtr`; the typed reader
            // owns the shape check and the generated function.
            if let Type::Fn(callable) = expected {
                if let Some((value, actual)) = prelowered.get_mut(index).and_then(Option::take) {
                    let (value, actual) = self.coerce_to_fn(
                        value,
                        actual,
                        expected,
                        &format!("argument {} to '{callee_name}'", index + 1),
                    )?;
                    self.check_value_argument_type(callee_name, index, expected, actual, Some(&argument.range))?;
                    args.push(Argument::Value(value));
                    continue;
                }
                self.seek_scalar_offset(argument.range.start);
                let converted = self.function_value_argument(callee_name, index + 1, callable, Some(argument), slots)?;
                args.push(converted);
                continue;
            }
            // The shared tree records `&x` as a borrow argument.
            // Only a borrow parameter consumes it; anywhere else the
            // typed route's expression parser rejects it (a pointer
            // cast is the only legal use of `&expr`).
            if matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                && !matches!(expected, Type::VectorRef(_) | Type::ArrayOf(..) | Type::StructRef(_) | Type::StructMut(_) | Type::Slice(_) | Type::SliceMut(_))
            {
                self.seek_scalar_offset(argument.range.start);
                return Err(self.fail("a borrow expression must be cast to a pointer"));
            }
            // A `&mut [T]` argument (ADR-0236 rung 4) borrows a whole vector
            // binding exclusively: the owner must be mutable, no other borrow
            // may be live in the statement, and the callee writes through the
            // fat pointer.
            if let Type::SliceMut(wanted) = expected {
                let wanted_elem = self.ctx.slice_elem(wanted);
                let ExpressionKind::BorrowArgument { name: borrowed, mutable, name_range, fields, index } = &argument.kind else {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail("a mutable slice argument is written '&mut name'"));
                };
                if !*mutable {
                    self.seek_scalar_offset(name_range.start);
                    return Err(self.fail("this parameter borrows mutably; write the argument as '&mut name'"));
                }
                if !fields.is_empty() {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("a mutable slice argument borrows a whole vector; field slices are not supported yet"));
                }
                // `&mut name[start..end]` windows the exclusive borrow; a single
                // element is not a slice.
                let range_bounds = index.as_ref().and_then(|index| ae_syntax::expression::split_range_argument(&self.tokens, index));
                if index.is_some() && range_bounds.is_none() {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("a single element is not a mutable slice; write '&mut name[start..end]'"));
                }
                let &(slot, binding_mutable, actual) = self.names.get(borrowed).ok_or_else(|| {
                    self.fail(&format!("unknown variable '{borrowed}'")).with_code("E0101")
                })?;
                // Forwarding a mutable slice deeper would need a cross-frame
                // owner model; reject it before the mutability check so the
                // diagnostic names the real limitation.
                if matches!(actual, Type::SliceMut(_) | Type::Slice(_)) {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("forwarding a mutable slice is not supported yet"));
                }
                if !binding_mutable {
                    return Err(self.immutable_binding_error(self.fail(&format!("cannot borrow '{borrowed}' mutably: it was declared without 'var'")).with_code("E0103"), slot));
                }
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                }
                let matches_element = match actual {
                    Type::Vector(id) => self.ctx.vector_elem(id) == wanted_elem,
                    _ => false,
                };
                if !matches_element {
                    return Err(self.fail(&format!("argument must be {}, found {}", expected.name(self.ctx), actual.name(self.ctx))));
                }
                if self.is_borrowed(slot) {
                    if !self.only_view_borrows_active(slot) {
                        return Err(self.fail(&format!("cannot borrow '{borrowed}' mutably while it is borrowed in this statement")));
                    }
                    self.deferred_write_conflicts.push((slot, name_range.start, borrowed.clone(), "mutate", None));
                }
                self.mut_borrows.push((slot, self.scopes.len()));
                if let Some((start_range, end_range)) = range_bounds {
                    if start_range.is_empty() || end_range.is_empty() {
                        self.seek_scalar_offset(argument.range.start);
                        return Err(self.fail("a slice range needs both bounds; write 'start..end'"));
                    }
                    let start = self.lower_slice_bound(start_range, slots)?;
                    let end = self.lower_slice_bound(end_range, slots)?;
                    self.seek_scalar_offset(argument.range.start);
                    let (line, column) = self.position();
                    args.push(Argument::Value(Expr::SliceRange(slot, actual, wanted_elem, Box::new(start), Box::new(end), line, column)));
                    continue;
                }
                self.seek_scalar_offset(argument.range.start);
                let (line, column) = self.position();
                args.push(Argument::Value(Expr::SliceOf(slot, actual, wanted_elem, line, column)));
                continue;
            }
            // A `&[T]` argument builds the fat pointer from a whole vector or
            // array binding, forwards a slice parameter, or names a sub-range
            // of any of the three (`&v[a..b]`, ADR-0236 rungs 1-3).
            if let Type::Slice(wanted) = expected {
                let wanted_elem = self.ctx.slice_elem(wanted);
                if let ExpressionKind::Atom(Kind::Ident(name)) = &argument.kind
                    && let Some(&(slot, _, actual @ Type::Slice(_))) = self.names.get(name)
                {
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"), slot)));
                    }
                    if let Type::Slice(found) = actual && self.ctx.slice_elem(found) != wanted_elem {
                        return Err(self.fail(&format!("argument must be {}, found {}", expected.name(self.ctx), actual.name(self.ctx))));
                    }
                    self.seek_scalar_offset(argument.range.start);
                    let (line, column) = self.position();
                    args.push(Argument::Value(Expr::SliceOf(slot, actual, wanted_elem, line, column)));
                    continue;
                }
                let ExpressionKind::BorrowArgument { name: borrowed, mutable, name_range: _, fields, index } = &argument.kind else {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail("a slice argument borrows a whole vector or array: write '&name'"));
                };
                if !fields.is_empty() {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("a slice argument borrows a whole vector or array; field slices are not supported yet"));
                }
                if *mutable {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("this parameter borrows read-only; write the argument as '&name'"));
                }
                // `&name[start..end]` is a sub-slice; a single `&name[index]`
                // borrows one element and is not a slice source.
                let range_bounds = index.as_ref().and_then(|index| ae_syntax::expression::split_range_argument(&self.tokens, index));
                if index.is_some() && range_bounds.is_none() {
                    self.seek_scalar_offset(argument.range.start);
                    return Err(self.fail("a slice argument borrows a whole vector or array; element slices are not supported yet"));
                }
                let &(slot, _, actual) = self.names.get(borrowed).ok_or_else(|| {
                    self.fail(&format!("unknown variable '{borrowed}'")).with_code("E0101")
                })?;
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                }
                let matches_element = match actual {
                    Type::Vector(id) | Type::VectorRef(id) => self.ctx.vector_elem(id) == wanted_elem,
                    Type::ArrayOf(id, _) => self.ctx.array_elem(id) == wanted_elem,
                    Type::Slice(id) => self.ctx.slice_elem(id) == wanted_elem,
                    _ => false,
                };
                if !matches_element {
                    return Err(self.fail(&format!("argument must be {}, found {}", expected.name(self.ctx), actual.name(self.ctx))));
                }
                if let Some((start_range, end_range)) = range_bounds {
                    if start_range.is_empty() || end_range.is_empty() {
                        self.seek_scalar_offset(argument.range.start);
                        return Err(self.fail("a slice range needs both bounds; write 'start..end'"));
                    }
                    let start = self.lower_slice_bound(start_range, slots)?;
                    let end = self.lower_slice_bound(end_range, slots)?;
                    if matches!(actual, Type::Vector(_) | Type::VectorRef(_)) && !self.is_borrowed(slot) {
                        self.call_borrows.push((slot, self.scopes.len()));
                    }
                    self.seek_scalar_offset(argument.range.start);
                    let (line, column) = self.position();
                    args.push(Argument::Value(Expr::SliceRange(slot, actual, wanted_elem, Box::new(start), Box::new(end), line, column)));
                    continue;
                }
                if matches!(actual, Type::Vector(_) | Type::VectorRef(_)) && !self.is_borrowed(slot) {
                    self.call_borrows.push((slot, self.scopes.len()));
                }
                self.seek_scalar_offset(argument.range.start);
                let (line, column) = self.position();
                args.push(Argument::Value(Expr::SliceOf(slot, actual, wanted_elem, line, column)));
                continue;
            }
            // A container or struct borrow takes the binding's
            // address (`Argument::Borrow`); the same classify rules,
            // messages and statement-scoped borrow registers the typed
            // route uses.
            if matches!(expected, Type::VectorRef(_) | Type::ArrayOf(..) | Type::StructRef(_) | Type::StructMut(_)) {
                // A binding already spelled as a borrow (`&vec`/`&Struct`, a
                // `for` element view) forwards its pointer unchanged: the
                // argument is the binding itself, not `&binding`.
                if let ExpressionKind::Atom(Kind::Ident(borrowed)) = &argument.kind
                    && let Some(&(slot, _, actual)) = self.names.get(borrowed)
                    && matches!(super::declarations::CallBorrow::classify(expected, actual), Some(super::declarations::CallBorrow::Forward))
                {
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                    }
                    args.push(Argument::Borrow(slot, false));
                    continue;
                }
                let ExpressionKind::BorrowArgument { name: borrowed, mutable: argument_mutability, name_range, fields, index: element_index } = &argument.kind else {
                    self.seek_scalar_offset(argument.range.end);
                    // A borrow argument names a binding or a place inside one
                    // (`&name`, `&name[index]`, `&name.field`); a bare element
                    // or field expression is not an addressable argument.
                    if matches!(argument.kind, ExpressionKind::Index { .. } | ExpressionKind::Field { .. }) {
                        return Err(self.fail("this parameter borrows a binding; pass '&name', '&name[index]' or '&name.field'"));
                    }
                    return Err(self.fail("expected Amp, found the argument"));
                };
                // A place borrow: `&name[index]`, `&name.field`, and
                // `&name.field[index]`. The place is addressed in place (a
                // struct/vector element read is a view), so the callee sees
                // the owner's storage; the borrow is read-only and lives only
                // for the call.
                if element_index.is_some() || !fields.is_empty() {
                    if *argument_mutability || matches!(expected, Type::StructMut(_)) {
                        self.seek_scalar_offset(name_range.start);
                        return Err(self.fail("a place borrow is read-only; '&mut name.field'/'&mut name[index]' is not supported yet"));
                    }
                    let &(slot, _, actual) = self.names.get(borrowed).ok_or_else(|| {
                        self.fail(&format!("unknown variable '{borrowed}'")).with_code("E0101")
                    })?;
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                    }
                    self.seek_scalar_offset(argument.range.start);
                    let (line, column) = self.position();
                    let mut ty = actual;
                    let mut place = Expr::Variable(slot, actual, line, column);
                    for (field, field_range) in fields {
                        let sid = match ty {
                            Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => sid,
                            other => {
                                return Err(self.error_at(field_range.start, &format!(
                                    "only a struct has fields, found {}", other.name(self.ctx),
                                )));
                            }
                        };
                        let fty = field_ty_in(sid, field, self.structs).ok_or_else(|| {
                            self.error_at(field_range.start, &format!("struct '{}' has no field '{field}'", self.structs[sid].name))
                        })?;
                        place = Expr::Field(Box::new(place), field.clone(), fty);
                        ty = fty;
                    }
                    if let Some(index_range) = element_index {
                        // The root may itself be a `&vec` parameter or a
                        // `&[T]` slice: indexing reads the storage the lender
                        // owns (ADR-0236 rung 2 borrows one slice element).
                        let (element, is_slice) = match ty {
                            Type::Vector(id) | Type::VectorRef(id) => (self.ctx.vector_elem(id), false),
                            Type::Slice(id) | Type::SliceMut(id) => (self.ctx.slice_elem(id), true),
                            other => {
                                return Err(self.error_at(name_range.start, &format!(
                                    "'&{borrowed}...' needs a vector place to index, found {}", other.name(self.ctx),
                                )));
                            }
                        };
                        let start = self.tokens.partition_point(|token| token.offset < index_range.start);
                        let Ok((index_expression, _)) = ae_syntax::expression::parse_expression_tokens(&self.file, self.source, &self.tokens, start, false) else {
                            return Err(self.error_at(index_range.start, "invalid index expression"));
                        };
                        self.seek_scalar_offset(index_range.start);
                        let (index_value, index_ty) = self.lower_scalar_body_expression(&index_expression, slots)?;
                        if index_ty != Type::Int(IntTy::I64) {
                            return Err(self.fail("vector index must be an integer"));
                        }
                        place = if is_slice {
                            Expr::SliceIndex(slot, Box::new(index_value), line, column, element)
                        } else {
                            Expr::VecIndexOf(Box::new(place), Box::new(index_value), line, column, element)
                        };
                        ty = element;
                    }
                    let borrows_place = match expected {
                        Type::StructRef(sid) => matches!(ty, Type::Struct(found) if found == sid),
                        Type::VectorRef(wanted) => matches!(ty, Type::Vector(found) if found == wanted),
                        _ => false,
                    };
                    if !borrows_place {
                        let shape = if ty == Type::String { Type::StrView } else { ty };
                        return Err(self.fail(&format!(
                            "argument must be {}, found {}",
                            expected.name(self.ctx),
                            shape.name(self.ctx),
                        )));
                    }
                    if !self.is_borrowed(slot) {
                        self.call_borrows.push((slot, self.scopes.len()));
                    }
                    args.push(Argument::Value(Expr::BorrowAs(Box::new(place), false, line, column)));
                    continue;
                }
                if matches!(expected, Type::StructMut(_)) && !*argument_mutability {
                    self.seek_scalar_offset(name_range.start);
                    return Err(self.fail("this parameter borrows mutably; write the argument as '&mut name'"));
                }
                self.seek_scalar_offset(argument.range.end);
                let &(slot, binding_mutable, actual) = self.names.get(borrowed).ok_or_else(|| {
                    self.fail(&format!("unknown variable '{borrowed}'")).with_code("E0101")
                })?;
                let Some(borrow) = super::declarations::CallBorrow::classify(expected, actual) else {
                    let shape = match expected {
                        Type::VectorRef(id) => Type::Vector(id),
                        _ => expected,
                    };
                    return Err(self.fail(&format!("argument must be {}, found {}", shape.name(self.ctx), actual.name(self.ctx))));
                };
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                }
                match (expected, borrow) {
                    (Type::StructRef(_), super::declarations::CallBorrow::Struct { address }) => {
                        if self.mut_borrows.iter().any(|&(bid, _)| bid == slot) {
                            return Err(self.fail(&format!("cannot borrow '{borrowed}' while it is mutably borrowed in this statement")));
                        }
                        // The borrow lives only as long as this call.
                        if !self.is_borrowed(slot) {
                            self.call_borrows.push((slot, self.scopes.len()));
                        }
                        args.push(Argument::Borrow(slot, address));
                    }
                    (Type::StructRef(_), super::declarations::CallBorrow::Forward) => {
                        args.push(Argument::Borrow(slot, false));
                    }
                    (Type::StructMut(_), super::declarations::CallBorrow::Struct { address }) => {
                        if !binding_mutable {
                            return Err(self.immutable_binding_error(self.fail(&format!("cannot borrow '{borrowed}' mutably: it was declared without 'var'")).with_code("E0103"), slot));
                        }
                        if self.is_borrowed(slot) {
                            // The callee may write any field, so the
                            // call releases the viewed bytes at the
                            // call point; with only view borrows
                            // active the conflict defers to the
                            // graph's ordered adjudication.
                            if !self.only_view_borrows_active(slot) {
                                return Err(self.fail(&format!("cannot borrow '{borrowed}' mutably while it is borrowed in this statement")));
                            }
                            // The conflict points at the borrowed name, the token the
                            // typed parser's cursor ends on (col of `d`
                            // in `&mut d`, not the `&`).
                            self.deferred_write_conflicts.push((slot, name_range.start, borrowed.clone(), "mutate", None));
                        }
                        self.mut_borrows.push((slot, self.scopes.len()));
                        args.push(Argument::Borrow(slot, address));
                    }
                    (Type::VectorRef(_) | Type::ArrayOf(..), borrow) => {
                        // An owned vector takes the address; a
                        // parameter already spelled `&vec` forwards
                        // its pointer unchanged.
                        args.push(Argument::Borrow(slot, borrow.address()));
                    }
                    _ => {
                        let shape = match expected {
                            Type::VectorRef(id) => Type::Vector(id),
                            _ => expected,
                        };
                        return Err(self.fail(&format!("argument must be {}, found {}", shape.name(self.ctx), actual.name(self.ctx))));
                    }
                }
                continue;
            }
            let (value, actual) = match prelowered.get_mut(index).and_then(Option::take) {
                Some(pair) => pair,
                None => self.lower_scalar_body_expression(argument, slots)?,
            };
            // A `&string` parameter is a read-only view: the argument
            // lowers as `Argument::View` and nothing moves (the typed
            // route's `typed_call_argument` for a `StrView`).
            if matches!(expected, Type::StrView) {
                if !matches!(actual, Type::String | Type::StrView) {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail(&format!("argument must be a string or string view, found {}", actual.name(self.ctx))));
                }
                args.push(Argument::View(value));
                continue;
            }
            let checked_self = if let (Some(atom), Expr::Variable(slot, ..)) = (receiver_value(argument), &value) {
                self.seek_scalar_offset(atom.range.end);
                self.check_variable_move(*slot)?;
                true
            } else {
                false
            };
            self.check_value_argument_type(callee_name, index, expected, actual, Some(&argument.range))?;
            // An owned aggregate passed by value moves: later uses
            // are rejected (E0102), and the drops pass trims the
            // Frees lists for this slot.
            if self.is_owned(actual)
                && let Expr::Variable(slot, ..) = &value {
                    if !checked_self {
                        self.check_variable_move(*slot)?;
                    }
                    self.moved.insert(*slot);
                }
            args.push(Argument::Value(value));
        }
        if args.len() != params.len() {
            return Err(self.fail(&format!("function '{callee_name}' expects {} arguments, got {}", params.len(), args.len())));
        }
        Ok(args)
    }

    /// `tokenize(text, sep)` from the shared nodes. Only the shapes the
    /// typed route accepts without further diagnostics (a string variable or
    /// a string field of a struct variable, a string/view separator) lower
    /// here; everything else returns None and keeps the typed parser's pinned
    /// messages.
    pub(super) fn lower_shared_tokenize(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        if arguments.len() != 2 {
            return Ok(None);
        }
        self.seek_scalar_offset(expression.range.start);
        let (line, col) = self.position();
        let (base, base_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
        if base_ty != Type::String {
            return Ok(None);
        }
        let (vid, field) = match &base {
            Expr::Variable(id, Type::String, _, _) => (*id, None),
            Expr::Field(inner, fname, Type::String) => match inner.as_ref() {
                Expr::Variable(id, Type::Struct(_), _, _) => (*id, Some(fname.clone())),
                _ => {
                    self.seek_scalar_offset(arguments[0].range.end);
                    return Err(self.fail("tokenize can follow a string field only on a struct variable"));
                }
            },
            _ => {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail("tokenize expects a string variable or a string field of a struct variable (the cursor borrows its base)"));
            }
        };
        if self.moved.contains(&vid) {
            self.seek_scalar_offset(arguments[0].range.start);
            return Err(moved_value_hint(self.binding_label(
                self.fail("cannot tokenize a moved string").with_code("E0102"), vid,
            )));
        }
        self.seek_scalar_offset(arguments[1].range.start);
        let (sep, sep_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
        if !matches!(sep_ty, Type::String | Type::StrView) {
            return Ok(None);
        }
        self.seek_scalar_offset(expression.range.end);
        let depth = self.scopes.len();
        if !self.borrows.iter().any(|&(id, _, _)| id == vid) {
            self.borrows.push((vid, depth, BorrowKind::View));
        }
        Ok(Some((Expr::Tokens(vid, field, Box::new(sep), line, col), Type::Tokens)))
    }

    /// An associated function (`Type::method(...)`) on a plain struct or
    /// enum: the inherent impl template supplies the signature, and the
    /// shared argument lowering supplies the arguments. Receiver-taking and
    /// missing entries answer with the typed route's own diagnostics; a
    /// private entry, or an argument shape the shared lowering does not
    /// cover, returns `Ok(None)` and keeps the typed delegation.
    pub(super) fn lower_shared_associated_call(
        &mut self,
        callee: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        expression: &ae_syntax::expression::Expression,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let ExpressionKind::Qualified { base, name: method } = &callee.kind else { return Ok(None) };
        let type_name = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => {
                if name == "Self" {
                    match self.impl_self {
                        Some(Type::Struct(sid)) => self.structs[sid].name.clone(),
                        Some(Type::Enum(name, _)) => name.to_owned(),
                        _ => return Ok(None),
                    }
                } else {
                    name.clone()
                }
            }
            // A generic instance (`Wrapper[i64]::new(..)`): the cached or
            // freshly instantiated layout names the impl template, and the
            // instance method supplies the signature.
            _ => return Ok(self.lower_shared_generic_associated_call(base, method, arguments, expression)),
        };
        let public = if let Some(&sid) = self.struct_names.get(&type_name) {
            let def = &self.structs[sid];
            def.public || def.file == self.file || def.file == PRELUDE
        } else if let Some(&eid) = self.enum_names.get(&type_name) {
            let def = &self.enums[eid];
            def.public || def.file == self.file || def.file == PRELUDE
        } else {
            return Ok(None);
        };
        if !public {
            return Ok(None);
        }
        let key = format!("{type_name}#{method}");
        match self.generics.inherent.get(&key).cloned() {
            Some(def) if def.receiver.is_some() => Err(self.error_at(callee.range.start, &format!(
                "'{type_name}::{method}' takes a receiver; call it as 'value.{method}(...)' instead"))),
            Some(def) if !def.signature.public => Ok(None),
            Some(def) => {
                let params = def.signature.params.clone();
                let mut nodes_left = 128;
                let mut effects = SharedExpressionEffects::default();
                let Some(args) = self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects) else {
                    return Ok(None);
                };
                self.commit_shared_effects(effects);
                self.seek_scalar_offset(expression.range.end);
                Ok(Some((Expr::Call(def.signature.id, args, def.signature.ret), def.signature.ret)))
            }
            None => {
                // The typed route's missing-associated-function diagnostic
                // lists the type's impl entries, receiver-bearing ones too.
                let prefix = format!("{type_name}#");
                let mut names: Vec<String> = self.generics.inherent.keys()
                    .filter_map(|entry| entry.strip_prefix(&prefix).map(str::to_owned))
                    .collect();
                names.sort();
                if names.is_empty() {
                    return Err(self.error_at(callee.range.end, &format!(
                        "type '{type_name}' has no methods; declare one in 'impl {type_name} {{ ... }}'")));
                }
                Err(self.error_at(callee.range.end, &format!(
                    "type '{type_name}' has no associated function '{method}'; it has {}",
                    names.join(", "))))
            }
        }
    }

    /// `Type[Args]::method(...)` on a generic struct or enum: instantiate the
    /// layout (cached first), find the associated function in the generic
    /// impl template, instantiate the method, and lower the arguments from
    /// the shared nodes. `None` keeps the typed diagnostics for a receiver
    /// entry, an unknown method or an argument shape the shared lowering does
    /// not cover.
    pub(super) fn lower_shared_generic_associated_call(
        &mut self,
        base: &ae_syntax::expression::Expression,
        method: &str,
        arguments: &[ae_syntax::expression::Expression],
        expression: &ae_syntax::expression::Expression,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;
        let (gname, type_ranges) = match &base.kind {
            ExpressionKind::Specialization { base, arguments }
            | ExpressionKind::BracketedCallTarget { base, arguments } => match &base.kind {
                ExpressionKind::Atom(Kind::Ident(name)) => (name.clone(), arguments.clone()),
                _ => return None,
            },
            _ => return None,
        };
        let shapes = self.loose_shape_arguments(&type_ranges)?;
        let type_args: Vec<Type> = shapes.iter().map(|shape| self.shape_type(shape, base.range.start)).collect::<Option<_>>()?;
        let instance = if let Some(&gid) = self.generics.struct_names.get(&gname) {
            let sid = match self.cached_struct_specialization(&gname, &type_ranges) {
                Some(sid) => sid,
                None => self.instantiate_struct(gid, type_args.clone(), base.range.start).ok()?,
            };
            let (origin_gid, origin_args) = self.generics.struct_origin.get(&sid).cloned()?;
            let gimpl = self.generics.generic_impls.get(&gname)?.clone();
            let method_def = gimpl.methods.iter().find(|entry| entry.name == method)?.clone();
            if method_def.receiver.is_some() {
                return None;
            }
            self.instantiate_impl_method(&gname, method, sid, origin_gid, &origin_args, base.range.start).ok()?
        } else if let Some(&gid) = self.generics.names.get(&gname) {
            let eid = match self.cached_enum_specialization(&gname, &type_ranges) {
                Some(eid) => eid,
                None => self.instantiate_enum(gid, type_args.clone(), base.range.start).ok()?,
            };
            let (origin_gid, origin_args) = self.generics.enum_origin.get(&eid).cloned()?;
            let gimpl = self.generics.generic_impls.get(&gname)?.clone();
            let method_def = gimpl.methods.iter().find(|entry| entry.name == method)?.clone();
            if method_def.receiver.is_some() {
                return None;
            }
            self.instantiate_impl_method(&gname, method, eid, origin_gid, &origin_args, base.range.start).ok()?
        } else {
            return None;
        };
        let (id, params, ret) = instance;
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        let args = self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.seek_scalar_offset(expression.range.end);
        Some((Expr::Call(id, args, ret), ret))
    }

    /// A variant path the shared tree spells `Enum::Variant` or
    /// `Enum::Variant(payload, ...)`: lower the payload from the shared nodes
    /// instead of handing the tokens back to the typed parser. `None` means
    /// the path names no variant (an associated call or function value), and
    /// the caller keeps the typed delegation for it.
    pub(super) fn lower_shared_enum_construction(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        // The shared variant lowering already answers generic instances
        // (`Option[i64]::Some`) and tracks argument effects; reuse it first.
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        if let Some((value, ty)) = self.lower_shared_variant(expression, arguments, 64, &mut nodes_left, &mut effects) {
            self.commit_shared_effects(effects);
            self.seek_scalar_offset(expression.range.end);
            return Ok(Some((value, ty)));
        }
        let ExpressionKind::Qualified { base, name: vname } = &expression.kind else {
            return Ok(None);
        };
        let eid = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(enum_name)) => {
                if enum_name == "Self" {
                    match self.impl_self {
                        Some(Type::Enum(_, eid)) => eid,
                        _ => return Ok(None),
                    }
                } else {
                    match self.enum_names.get(enum_name) {
                        Some(&eid) => eid,
                        None => return Ok(None),
                    }
                }
            }
            // A generic instance (`Option[i64]::Some(..)`): the cached
            // instance answers first; otherwise instantiate it here, exactly
            // like the shared struct-literal arm. A shape the reader cannot
            // finish returns None and keeps the typed delegation.
            ExpressionKind::Specialization { base, arguments }
            | ExpressionKind::BracketedCallTarget { base, arguments } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return Ok(None);
                };
                if let Some(eid) = self.cached_enum_specialization(name, arguments) {
                    eid
                } else {
                    let Some(&gid) = self.generics.names.get(name) else {
                        return Ok(None);
                    };
                    let args = if let Some(shapes) = self.loose_shape_arguments(arguments) {
                        let Some(args) = shapes.iter().map(|shape| self.shape_type(shape, base.range.start)).collect::<Option<Vec<_>>>() else {
                            return Ok(None);
                        };
                        args
                    } else {
                        let mut parsed = Vec::with_capacity(arguments.len());
                        for arg in arguments {
                            self.seek_scalar_offset(arg.start);
                            parsed.push(self.named_type()?);
                        }
                        parsed
                    };
                    // The typed route admits only the flat payload-capable
                    // arguments for a generic enum; a struct or enum argument
                    // is a diagnostic, not an instantiation.
                    if let Some((index, _)) = args.iter().enumerate().find(|(_, ty)| !matches!(ty, Type::Int(_) | Type::Bool | Type::Float(FloatTy::F64) | Type::Char | Type::Unit | Type::String | Type::Vector(_) | Type::Fn(_) | Type::Enum(..))) {
                        return Err(self.error_at(arguments[index].start, "type arguments must be i64, bool, f64, char, string, vec, vec[string], fn(...), or ()"));
                    }
                    self.instantiate_enum(gid, args, base.range.start)?
                }
            }
            _ => return Ok(None),
        };
        let def = self.enums[eid].clone();
        let Some((vidx, fields)) = def.variant_payload(vname) else {
            return Ok(None);
        };
        self.require_public("enum", def.name, &def.file, def.public)?;
        if arguments.len() > fields.len() {
            self.seek_scalar_offset(arguments[fields.len()].range.start);
            return Err(self.fail(&format!("variant '{}::{}' takes {} payload value(s)", def.name, vname, fields.len())));
        }
        let mut values = Vec::with_capacity(fields.len());
        for (index, field) in fields.iter().enumerate() {
            let Some(argument) = arguments.get(index) else {
                // A unit payload may be omitted: `StatusResult::Ok` and
                // `Result[(), string]::Ok` both mean `Ok(())` (P2.4).
                if fields.len() == 1 && *field == Type::Unit {
                    values.push(Expr::Unit);
                    continue;
                }
                self.seek_scalar_offset(expression.range.end);
                return Err(self.fail(&format!("variant '{}::{}' needs {} payload value(s)", def.name, vname, fields.len())));
            };
            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
            let (value, actual) = self.coerce_to_fn(value, actual, *field, &format!("payload {} of '{}::{}'", index + 1, def.name, vname))?;
            if !self.matches_value_type(*field, actual, Some(&argument.range)) {
                self.seek_scalar_offset(argument.range.end);
                return Err(self.fail(&format!("payload {} of '{}::{}' must be {}", index + 1, def.name, vname, field.name(self.ctx))));
            }
            // An owned payload value moves into the variant, exactly like an
            // owned struct field: the source slot's Frees is trimmed.
            if self.is_owned(actual)
                && let Expr::Variable(slot, ..) = &value {
                    self.check_variable_move(*slot)?;
                    self.moved.insert(*slot);
                }
            values.push(value);
        }
        self.seek_scalar_offset(expression.range.end);
        Ok(Some((Expr::EnumLit(def.name, eid, vidx, values), Type::Enum(def.name, eid))))
    }

    pub(super) fn coerce_to_fn(
        &self,
        expr: Expr,
        actual: Type,
        expected: Type,
        context: &str,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        if actual == expected {
            return Ok((expr, actual));
        }
        let Type::Fn(sig) = expected else {
            return Ok((expr, actual));
        };
        let cid = match (&expr, actual) {
            (Expr::ClosureLit(cid), _) => *cid,
            (_, Type::Closure(cid)) => cid,
            _ => return Ok((expr, actual)),
        };
        let def = &self.generics.closures[cid];
        if !def.value_captures.is_empty() || !def.borrow_captures.is_empty() {
            return Err(self.fail("a closure with captures cannot be passed as a function value; make it parameter-only or bind its inputs as arguments"));
        }
        let (params, returns) = self.ctx.fn_sig(sig);
        if def.params != params || def.ret != returns {
            let got = format!(
                "fn({}) -> {}",
                def.params.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", "),
                def.ret.name(self.ctx),
            );
            return Err(self.fail(&format!("{context}: expected {}, found {got}", expected.name(self.ctx))));
        }
        Ok((Expr::FnRef(def.fn_id, sig), expected))
    }

    fn lower_fn_sig_call_args(
        &mut self,
        callee_name: &str,
        sig: usize,
        _expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Vec<Argument>, Type), ae_span::Diagnostic> {
        let (params, ret) = self.ctx.fn_sig(sig);
        let mut args = Vec::with_capacity(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            self.seek_scalar_offset(argument.range.start);
            let Some(expected) = params.get(index).copied() else {
                return Err(self.fail(&format!("too many arguments to '{callee_name}'")));
            };
            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
            let (value, actual) = self.coerce_to_fn(
                value,
                actual,
                expected,
                &format!("argument {} to '{callee_name}'", index + 1),
            )?;
            if matches!(expected, Type::StrView) {
                if !matches!(actual, Type::String | Type::StrView) {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail(&format!(
                        "argument must be a string or string view, found {}",
                        actual.name(self.ctx)
                    )));
                }
                args.push(Argument::View(value));
                continue;
            }
            self.check_value_argument_type(callee_name, index, expected, actual, Some(&argument.range))?;
            if self.is_owned(actual)
                && let Expr::Variable(source, ..) = &value
            {
                self.moved.insert(*source);
            }
            args.push(Argument::Value(value));
        }
        if args.len() != params.len() {
            return Err(self.fail(&format!(
                "function '{callee_name}' expects {} arguments, got {}",
                params.len(),
                args.len()
            )));
        }
        Ok((args, ret))
    }

    /// Resolve the target type of a borrow argument's place chain (`&x`,
    /// `&x.field`, `&x[i]`, `&x.field[i]`) without mutating borrow or move
    /// state, for generic call-site type inference.
    fn resolve_borrow_place_type(
        &self,
        base_ty: Type,
        fields: &[(String, std::ops::Range<usize>)],
        has_index: bool,
    ) -> Option<Type> {
        let mut ty = base_ty;
        for (field, _) in fields {
            let sid = match ty {
                Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => sid,
                _ => return None,
            };
            ty = field_ty_in(sid, field, self.structs)?;
        }
        if has_index {
            ty = match ty {
                Type::Vector(id) | Type::VectorRef(id) => self.ctx.vector_elem(id),
                Type::Slice(id) | Type::SliceMut(id) => self.ctx.slice_elem(id),
                Type::ArrayOf(id, _) => self.ctx.array_elem(id),
                _ => return None,
            };
        }
        Some(ty)
    }

    fn try_lower_inferred_generic_call(
        &mut self,
        name: &str,
        gid: usize,
        call_offset: usize,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Option<(Expr, Type)>, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;

        let def = self.generics.fn_defs[gid].clone();
        let callable_inference = def.bounds.iter().flatten().any(|b| matches!(b, Bound::Callable(..)));
        let Some(bindings) = def.bindings.as_ref() else {
            return Ok(None);
        };
        // P2.5.2 infers from direct value parameters (`T`), borrow parameters
        // (`&T`, `&mut T`), slice parameters (`&[T]`, `&mut [T]`), and vector
        // parameters (`vec[T]`). Every type parameter needs at least one
        // inferable parameter position; return-only or nested composite
        // parameters stay on the explicit-type-argument path.
        let patterns: Vec<ParamPattern> = bindings
            .iter()
            .map(|binding| classify_generic_param_pattern(&def.source[binding.ty.clone()], &def.params))
            .collect();
        let every_param_covered = (0..def.params.len()).all(|tp| {
            patterns.iter().any(|p| p.type_param() == Some(tp))
        });
        if !callable_inference && !every_param_covered {
            return Ok(None);
        }
        if arguments.len() != bindings.len() {
            return Err(self.error_at(call_offset, &format!(
                "function '{name}' expects {} arguments, got {}",
                bindings.len(),
                arguments.len()
            )));
        }

        let mut prelowered: Vec<Option<(Expr, Type)>> = vec![None; arguments.len()];
        let mut inferred: Vec<Option<Type>> = vec![None; def.params.len()];

        for (index, (argument, &pattern)) in arguments.iter().zip(&patterns).enumerate() {
            self.seek_scalar_offset(argument.range.start);
            if let ExpressionKind::BorrowArgument { name: arg_name, fields, index: elem_index, .. } = &argument.kind {
                // Borrow arguments are inspected read-only here and lowered
                // after instantiation by `lower_value_arguments_with`, so all
                // mutability, move, and borrow-conflict checks run once.
                let Some(&(_, _, base_ty)) = self.names.get(arg_name) else {
                    continue;
                };
                let candidate = match pattern {
                    ParamPattern::Slice(tp) | ParamPattern::SliceMut(tp) if fields.is_empty() => {
                        let elem = match base_ty {
                            Type::Vector(id) | Type::VectorRef(id) => Some(self.ctx.vector_elem(id)),
                            Type::ArrayOf(id, _) => Some(self.ctx.array_elem(id)),
                            Type::Slice(id) | Type::SliceMut(id) => Some(self.ctx.slice_elem(id)),
                            _ => None,
                        };
                        elem.map(|ty| (tp, ty))
                    }
                    ParamPattern::Borrow(tp) | ParamPattern::BorrowMut(tp) => {
                        self.resolve_borrow_place_type(base_ty, fields, elem_index.is_some()).map(|place_ty| {
                            let target = match place_ty {
                                Type::StructRef(sid) | Type::StructMut(sid) => Type::Struct(sid),
                                Type::VectorRef(id) => Type::Vector(id),
                                other => other,
                            };
                            (tp, target)
                        })
                    }
                    _ => None,
                };
                if let Some((tp, ty)) = candidate && inferred[tp].is_none() {
                    inferred[tp] = Some(ty);
                }
                continue;
            }

            let (mut expr, mut ty) = self.lower_scalar_body_expression(argument, slots)?;
            match pattern {
                ParamPattern::Value(tp) => {
                    let callable_req = def.bounds[tp].iter().find_map(|b| match b {
                        Bound::Callable(kind, sig) => Some((*kind, *sig)),
                        _ => None,
                    });
                    if let Some((req_kind, req_sig)) = callable_req {
                        let cid = match (&expr, ty) {
                            (Expr::ClosureLit(cid), _) => Some(*cid),
                            (_, Type::Closure(cid)) => Some(cid),
                            _ => None,
                        };
                        if let Some(cid) = cid {
                            let (struct_lit, env_info) = self.materialize_closure_struct_lit(
                                cid,
                                None,
                                argument.range.start,
                            )?;
                            if env_info.sig != req_sig || !env_info.kind.satisfies(req_kind) {
                                return Err(self.error_at(
                                    argument.range.start,
                                    &format!("closure does not satisfy '{}({})' constraint", req_kind.name(), name),
                                ));
                            }
                            expr = struct_lit;
                            ty = Type::Struct(env_info.env_sid);
                        }
                    }
                    if inferred[tp].is_none() {
                        inferred[tp] = Some(ty);
                    }
                }
                ParamPattern::Borrow(tp) => {
                    let target = match ty {
                        Type::String | Type::StrView => Type::String,
                        Type::StructRef(sid) | Type::StructMut(sid) | Type::Struct(sid) => Type::Struct(sid),
                        Type::VectorRef(id) | Type::Vector(id) => Type::Vector(id),
                        other => other,
                    };
                    if inferred[tp].is_none() {
                        inferred[tp] = Some(target);
                    }
                }
                ParamPattern::BorrowMut(tp) => {
                    let target = match ty {
                        Type::StructMut(sid) | Type::Struct(sid) => Type::Struct(sid),
                        other => other,
                    };
                    if inferred[tp].is_none() {
                        inferred[tp] = Some(target);
                    }
                }
                ParamPattern::Slice(tp) => {
                    if let Type::Slice(id) = ty && inferred[tp].is_none() {
                        inferred[tp] = Some(self.ctx.slice_elem(id));
                    }
                }
                ParamPattern::Vec(tp) => {
                    if let Type::Vector(id) = ty && inferred[tp].is_none() {
                        inferred[tp] = Some(self.ctx.vector_elem(id));
                    }
                }
                ParamPattern::SliceMut(_) | ParamPattern::Fixed => {}
            }
            prelowered[index] = Some((expr, ty));
        }

        if inferred.iter().any(Option::is_none) {
            return Ok(None);
        }
        let type_args: Vec<Type> = inferred.into_iter().map(Option::unwrap).collect();
        let (inst_id, inst_params, inst_ret) = self.instantiate_fn(gid, type_args, call_offset)?;
        let final_args = self.lower_value_arguments_with(name, arguments, &inst_params, slots, prelowered)?;
        Ok(Some((Expr::Call(inst_id, final_args, inst_ret), inst_ret)))
    }
}

#[derive(Clone, Copy)]
enum ParamPattern {
    Value(usize),
    Borrow(usize),
    BorrowMut(usize),
    Slice(usize),
    SliceMut(usize),
    Vec(usize),
    Fixed,
}

impl ParamPattern {
    fn type_param(self) -> Option<usize> {
        match self {
            Self::Value(tp)
            | Self::Borrow(tp)
            | Self::BorrowMut(tp)
            | Self::Slice(tp)
            | Self::SliceMut(tp)
            | Self::Vec(tp) => Some(tp),
            Self::Fixed => None,
        }
    }
}

fn classify_generic_param_pattern(raw: &str, params: &[String]) -> ParamPattern {
    let raw = raw.trim();
    if let Some(tp) = params.iter().position(|p| p == raw) {
        return ParamPattern::Value(tp);
    }
    if let Some(inner) = raw.strip_prefix("&mut ").map(str::trim) {
        if let Some(elem) = inner.strip_prefix('[').and_then(|s| s.strip_suffix(']')).map(str::trim) {
            if let Some(tp) = params.iter().position(|p| p == elem) {
                return ParamPattern::SliceMut(tp);
            }
        } else if let Some(tp) = params.iter().position(|p| p == inner) {
            return ParamPattern::BorrowMut(tp);
        }
        return ParamPattern::Fixed;
    }
    if let Some(inner) = raw.strip_prefix('&').map(str::trim) {
        if let Some(elem) = inner.strip_prefix('[').and_then(|s| s.strip_suffix(']')).map(str::trim) {
            if let Some(tp) = params.iter().position(|p| p == elem) {
                return ParamPattern::Slice(tp);
            }
        } else if let Some(tp) = params.iter().position(|p| p == inner) {
            return ParamPattern::Borrow(tp);
        }
        return ParamPattern::Fixed;
    }
    if let Some(inner) = raw.strip_prefix("vec[").and_then(|s| s.strip_suffix(']')).map(str::trim)
        && let Some(tp) = params.iter().position(|p| p == inner)
    {
        return ParamPattern::Vec(tp);
    }
    ParamPattern::Fixed
}

/// Resolve a struct-field receiver chain (`a.b.cells`) to its root binding
/// and the field path with each field's declared type. The root's slot and
/// declared type come from the binding table; every step names a real field.
pub(super) struct FieldChain {
    pub(super) root: String,
    pub(super) slot: usize,
    pub(super) root_ty: Type,
    pub(super) path: Vec<(String, Type)>,
}

pub(super) fn resolve_field_chain(
    parser: &Parser<'_>,
    target: &ae_syntax::expression::Expression,
) -> Result<FieldChain, ae_span::Diagnostic> {
    use ae_syntax::expression::ExpressionKind;
    let mut fields = Vec::new();
    let mut node = target;
    loop {
        match &node.kind {
            ExpressionKind::Field { base, name } => {
                fields.push((name.clone(), node.range.clone()));
                node = base;
            }
            ExpressionKind::Atom(Kind::Ident(root)) => {
                let &(slot, _, root_ty) = parser.names.get(root).ok_or_else(|| {
                    parser.error_at(node.range.start, &format!("unknown variable '{root}'")).with_code("E0101")
                })?;
                fields.reverse();
                let mut path = Vec::new();
                let mut current = root_ty;
                for (name, range) in fields {
                    let sid = match current {
                        Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => sid,
                        other => {
                            return Err(parser.error_at(range.start, &format!(
                                "only a struct has fields, found {}", other.name(parser.ctx),
                            )));
                        }
                    };
                    let fty = field_ty_in(sid, &name, parser.structs).ok_or_else(|| {
                        parser.error_at(range.start, &format!("struct '{}' has no field '{name}'", parser.structs[sid].name))
                    })?;
                    path.push((name, fty));
                    current = fty;
                }
                return Ok(FieldChain { root: root.clone(), slot, root_ty, path });
            }
            _ => return Err(parser.error_at(node.range.start, "a container receiver needs a named field chain")),
        }
    }
}

/// The type an element read hands back: a view for owned storage (a string
/// element, a nested vector, an owned struct) and the element value itself
/// for Copy elements.
pub(super) fn vector_element_read_type(element: Type, structs: &[StructDef]) -> Type {
    match element {
        Type::String => Type::StrView,
        Type::Vector(inner) => Type::VectorRef(inner),
        Type::Struct(sid) if !structs[sid].is_copy => Type::StructRef(sid),
        other => other,
    }
}
