//! Lowering module: expr (P1.4 parser split).
use super::*;
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
            ExpressionKind::Atom(Kind::Number(value)) => (Expr::Number(*value), Type::Int),
            ExpressionKind::Atom(Kind::Float(value)) => (Expr::FloatLit(*value), Type::F64),
            ExpressionKind::Atom(Kind::True) => (Expr::BoolLit(true), Type::Bool),
            ExpressionKind::Atom(Kind::False) => (Expr::BoolLit(false), Type::Bool),
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
                let Some(def) = self.resolved_expressions().scalar_reads.get(&expression.range.start).copied() else {
                    // A generic enum template in value position has the typed
                    // route's own reminder; a plain enum name has none (the
                    // typed arm reports it as an unknown variable).
                    if self.generics.names.contains_key(name) {
                        return Err(self.error_at(expression.range.start, &format!("'{name}' is a generic enum; write '{name}[string]::Variant'")));
                    }
                    if self.struct_names.contains_key(name) {
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail(&format!("'{name}' is a struct name; construct it with '{name} {{ ... }}'")));
                    }
                    return Err(self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101"));
                };
                let &(slot, ty) = slots.get(&def).ok_or_else(|| {
                    self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101")
                })?;
                self.seek_scalar_offset(expression.range.start);
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"),
                        slot,
                    )));
                }
                let (line, column) = self.position();
                (Expr::Variable(slot, ty, line, column), ty)
            }
            ExpressionKind::Group(inner) => self.lower_scalar_body_expression(inner, slots)?,
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
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_)) {
                    self.seek_scalar_offset(expression.range.start);
                    return Err(self.fail(&format!("'as' retags a raw pointer, and {} is not a pointer", base_type.name(self.ctx))));
                }
                let name = self.source.get(pointee.clone())
                    .ok_or_else(|| self.error_at(pointee.start, "cannot read the pointer's pointee type"))?;
                let pointee_type = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))
                    .ok_or_else(|| self.error_at(pointee.start, "a pointer cast retags to a scalar pointee here"))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end).ok_or_else(|| self.fail("expected the end of the pointer cast"))?;
                let id = self.ctx.intern_pointee(pointee_type);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                (Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty)
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
                            let shapes = self.loose_shape_arguments(arguments)
                                .ok_or_else(|| self.error_at(target.range.start, &format!("cannot read the type arguments of '{name}'")))?;
                            let args = shapes.iter().map(|shape| self.shape_type(shape, target.range.start))
                                .collect::<Option<Vec<_>>>()
                                .ok_or_else(|| self.error_at(target.range.start, &format!("cannot read the type arguments of '{name}'")))?;
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
                    self.check_field_value_type(&field.name, expected, actual, Some(&field.value.range))?;
                    // An owned field value moves into the struct: the literal
                    // owns it, so the source slot's Frees is trimmed (the same
                    // rule the typed struct literal applies).
                    if actual.owned()
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
                    let Some(&def) = self.resolved_expressions().scalar_reads.get(&base.range.start) else {
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
                    let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                    let min = self.tokens.get(end)
                        .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                        .map_or(0, |(left, _)| left + 1);
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.fallback_expression(min, true)?;
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
                if let Some((value, ty)) = self.lower_shared_associated_call(callee, arguments, expression) {
                    return Ok((value, ty));
                }
                // An associated call (`Point::new(1)`): the typed postfix layer
                // builds the instance and owns the pinned diagnostics. Bounded
                // by the following operator, like a chained method receiver.
                if let ExpressionKind::Qualified { .. } = &callee.kind {
                    let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                    let min = self.tokens.get(end)
                        .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                        .map_or(0, |(left, _)| left + 1);
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.fallback_expression(min, true)?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok(parsed);
                }
                if let ExpressionKind::Field { base, name: method_name } = &callee.kind {
                    // A borrow argument (`pair.combine(&mut pair)`): the typed
                    // postfix layer classifies the argument and rejects an
                    // alias of the receiver slot in one statement. Delegate the
                    // whole call, bounded by the following operator.
                    if arguments.iter().any(|argument| matches!(argument.kind, ExpressionKind::BorrowArgument { .. })) {
                        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                        let min = self.tokens.get(end)
                            .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                            .map_or(0, |(left, _)| left + 1);
                        self.seek_scalar_offset(expression.range.start);
                        let parsed = self.fallback_expression(min, true)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok(parsed);
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
                        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                        let min = self.tokens.get(end)
                            .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                            .map_or(0, |(left, _)| left + 1);
                        self.seek_scalar_offset(expression.range.start);
                        let parsed = self.fallback_expression(min, true)?;
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
                        if count_ty != Type::Int {
                            self.seek_scalar_offset(count.range.end);
                            return Err(self.fail(&format!("{method_name} takes an i64 count")));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        let (line, column) = self.position();
                        let bytewise = method_name == "byte_offset";
                        return Ok((Expr::PtrOffset(Box::new(operand), Box::new(count_value), bytewise, line, column), ty));
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
                if let ExpressionKind::BracketedCallTarget { base, .. } = &callee.kind {
                    let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                        return Err(self.fail("this call form is not supported by the body route"));
                    };
                    // The C layout queries are spelled like a generic call but read a
                    // repr(C) layout: cursor on the `[` for the shared reader.
                    if matches!(name.as_str(), "c_size_of" | "c_align_of" | "c_offset_of") {
                        self.seek_scalar_offset(base.range.start);
                        self.pos += 1;
                        let (value, ty) = self.c_layout_expr(name)?;
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
                    // against the substituted signature with the shared
                    // argument rules (borrows, views, moves).
                    let budget = 128usize.checked_sub(self.expression_nodes)
                        .ok_or_else(|| self.fail("expression node limit exceeded"))?;
                    let remaining = 128usize.checked_sub(self.expression_depth).map(|depth| depth.min(64))
                        .ok_or_else(|| self.fail("expression depth limit exceeded"))?;
                    let mut nodes_left = budget;
                    let mut effects = SharedExpressionEffects::default();
                    let shared = self.lower_shared_call_arguments(arguments, &params, remaining, &mut nodes_left, &mut effects);
                    let args = match shared {
                        Some(args) => {
                            self.commit_shared_effects(effects);
                            self.expression_nodes += budget - nodes_left;
                            args
                        }
                        // The generic template's own argument list may not be
                        // indexed in the shared tree; the typed argument
                        // reader still owns the exact diagnostics (and its
                        // error falls the whole body back to the typed route).
                        None => {
                            let open = self.tokens.partition_point(|token| token.offset < callee.range.end);
                            self.pos = open;
                            self.call_arguments(name, &params)?
                        }
                    };
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
                        let (value, ty) = self.closure_call(name, callee.range.start)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((value, ty));
                    }
                    // A call through a function-typed value: the binding's
                    // slot names the pointer the caller passed (ADR-0189).
                    if let Some(&(slot, _, Type::Fn(sig))) = self.names.get(name) {
                        let (params, ret) = self.ctx.fn_sig(sig);
                        // A borrow argument needs the typed parser's classify
                        // rules (an alias of the same slot in one statement is
                        // rejected); delegate the whole call, bounded by the
                        // following operator.
                        if arguments.iter().any(|argument| matches!(argument.kind, ExpressionKind::BorrowArgument { .. })) {
                            let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                            let min = self.tokens.get(end)
                                .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                                .map_or(0, |(left, _)| left + 1);
                            self.seek_scalar_offset(expression.range.start);
                            let parsed = self.fallback_expression(min, true)?;
                            self.seek_scalar_offset(expression.range.end);
                            return Ok(parsed);
                        }
                        let mut args = Vec::with_capacity(arguments.len());
                        for (index, argument) in arguments.iter().enumerate() {
                            self.seek_scalar_offset(argument.range.start);
                            let Some(expected) = params.get(index).copied() else {
                                return Err(self.fail(&format!("too many arguments to '{name}'")));
                            };
                            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                            // A `&string` parameter takes a view (the typed
                            // `typed_call_argument` rule); everything else is
                            // a value, and an owned one moves.
                            if matches!(expected, Type::StrView) {
                                if !matches!(actual, Type::String | Type::StrView) {
                                    self.seek_scalar_offset(argument.range.end);
                                    return Err(self.fail(&format!("argument must be a string or string view, found {}", actual.name(self.ctx))));
                                }
                                args.push(Argument::View(value));
                                continue;
                            }
                            self.check_value_argument_type(name, index, expected, actual, Some(&argument.range))?;
                            if actual.owned()
                                && let Expr::Variable(source, ..) = &value {
                                    self.moved.insert(*source);
                                }
                            args.push(Argument::Value(value));
                        }
                        if args.len() != params.len() {
                            return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", params.len(), args.len())));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::FnCall(slot, args, ret, sig), ret));
                    }
                }
                let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else {
                    // Only a bare name or a field receiver reaches this arm;
                    // anything else is a call form the route does not take.
                    return Err(self.fail("this call form is not supported by the body route"));
                };
                let signature = match self.functions.get(name).cloned() {
                    Some(signature) => signature,
                    // A bare call to a generic function: the type arguments are
                    // mandatory (there is no inference), the typed route's
                    // message.
                    None if self.generics.fn_names.contains_key(name) => {
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
                let mut args = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    self.seek_scalar_offset(argument.range.start);
                    if index >= 64 {
                        return Err(self.fail("function argument limit exceeded (64)"));
                    }
                    let &expected = signature.params.get(index)
                        .ok_or_else(|| self.fail(&format!("too many arguments to '{name}'")))?;
                    // A function-typed parameter takes a closure literal (or
                    // a named function) as `Argument::FnPtr`; the typed reader
                    // owns the shape check and the generated function.
                    if let Type::Fn(callable) = expected {
                        self.seek_scalar_offset(argument.range.start);
                        let converted = self.function_value_argument(name, index + 1, callable)?;
                        args.push(converted);
                        continue;
                    }
                    // The shared tree records `&x` as a borrow argument.
                    // Only a borrow parameter consumes it; anywhere else the
                    // typed route's expression parser rejects it (a pointer
                    // cast is the only legal use of `&expr`).
                    if matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                        && !matches!(expected, Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_))
                    {
                        self.seek_scalar_offset(argument.range.start);
                        return Err(self.fail("a borrow expression must be cast to a pointer"));
                    }
                    // A container or struct borrow takes the binding's
                    // address (`Argument::Borrow`); the same classify rules,
                    // messages and statement-scoped borrow registers the typed
                    // route uses.
                    if matches!(expected, Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_)) {
                        let ExpressionKind::BorrowArgument { name: borrowed, mutable: argument_mutability, name_range, .. } = &argument.kind else {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail("expected Amp, found the argument"));
                        };
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
                                Type::VecRef => Type::Vec, Type::StrVecRef => Type::StrVec, Type::VecFRef => Type::VecF, _ => expected,
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
                            (Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_), borrow) => {
                                // An owned vector takes the address; a
                                // parameter already spelled `&vec` forwards
                                // its pointer unchanged.
                                args.push(Argument::Borrow(slot, borrow.address()));
                            }
                            _ => {
                                let shape = match expected {
                                    Type::VecRef => Type::Vec, Type::StrVecRef => Type::StrVec, Type::VecFRef => Type::VecF, _ => expected,
                                };
                                return Err(self.fail(&format!("argument must be {}, found {}", shape.name(self.ctx), actual.name(self.ctx))));
                            }
                        }
                        continue;
                    }
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
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
                    if let (Some(atom), Expr::Variable(slot, ..)) = (receiver_value(argument), &value) {
                        self.seek_scalar_offset(atom.range.end);
                        self.check_variable_move(*slot)?;
                    }
                    self.check_value_argument_type(name, index, expected, actual, Some(&argument.range))?;
                    // An owned aggregate passed by value moves: later uses
                    // are rejected (E0102), and the drops pass trims the
                    // Frees lists for this slot.
                    if actual.owned()
                        && let Expr::Variable(slot, ..) = &value {
                            self.moved.insert(*slot);
                        }
                    args.push(Argument::Value(value));
                }
                self.seek_scalar_offset(expression.range.end);
                if args.len() != signature.params.len() {
                    return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", signature.params.len(), args.len())));
                }
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
                let parsed = self.fallback_expression(0, false)?;
                self.seek_scalar_offset(expression.range.end);
                return Ok(parsed);
            }
            ExpressionKind::Specialization { .. } => {
                // A bracket the syntax tree kept as a specialization on a
                // value name (`arr[j]`): the typed expression parser
                // disambiguates it against the known names and indexes.
                self.seek_scalar_offset(expression.range.start);
                self.fallback_expression(0, false)?
            }
            ExpressionKind::Index { base, index } => {
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
                    Type::Array(len) => {
                        let value = self.lower_scalar_array_index(index, slots)?;
                        (Expr::Index(slot, len, Box::new(value), line, column), Type::Int)
                    }
                    Type::VecRef | Type::VecFRef | Type::Vec | Type::VecF => {
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let element = if matches!(ty, Type::VecFRef | Type::VecF) { Type::F64 } else { Type::Int };
                        let borrowed = matches!(ty, Type::VecRef | Type::VecFRef);
                        (Expr::VecIndex(slot, Box::new(value), line, column, borrowed, element), element)
                    }
                    Type::StrVecRef | Type::StrVec => {
                        // A borrowed string vector's element is a view; an
                        // owned one hands the element's view too.
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let by_ref = matches!(ty, Type::StrVecRef);
                        (Expr::StrVecIndex(slot, Box::new(value), line, column, by_ref), Type::StrView)
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
                    element_type = Some(self.check_numeric_vector_element(element_type, ty)?);
                    values.push(value);
                }
                let ty = if element_type == Some(Type::F64) { Type::VecF } else { Type::Vec };
                (Expr::VecLit(values, ty, line, column), ty)
            }
            _ => return Err(self.error_at(expression.range.start, "this expression is not supported by the body route")),
        })
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
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        if self.moved.contains(&vid) {
            return Ok(None);
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
    /// shared argument lowering supplies the arguments. `None` leaves the
    /// typed postfix layer to produce its own diagnostics (a receiver-taking
    /// entry, a private item, an unknown method, or a shape the shared
    /// argument lowering does not cover).
    pub(super) fn lower_shared_associated_call(
        &mut self,
        callee: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        expression: &ae_syntax::expression::Expression,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;
        let ExpressionKind::Qualified { base, name: method } = &callee.kind else { return None };
        let type_name = match &base.kind {
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
            // A generic instance (`Wrapper[i64]::new(..)`): the cached or
            // freshly instantiated layout names the impl template, and the
            // instance method supplies the signature.
            _ => return self.lower_shared_generic_associated_call(base, method, arguments, expression),
        };
        let public = if let Some(&sid) = self.struct_names.get(&type_name) {
            let def = &self.structs[sid];
            def.public || def.file == self.file || def.file == PRELUDE
        } else if let Some(&eid) = self.enum_names.get(&type_name) {
            let def = &self.enums[eid];
            def.public || def.file == self.file || def.file == PRELUDE
        } else {
            return None;
        };
        if !public {
            return None;
        }
        let key = format!("{type_name}#{method}");
        let def = self.generics.inherent.get(&key)?.clone();
        if def.receiver.is_some() || !def.signature.public {
            return None;
        }
        let params = def.signature.params.clone();
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        let args = self.lower_shared_call_arguments(arguments, &params, 64, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.seek_scalar_offset(expression.range.end);
        Some((Expr::Call(def.signature.id, args, def.signature.ret), def.signature.ret))
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
                    let Some(shapes) = self.loose_shape_arguments(arguments) else {
                        return Ok(None);
                    };
                    let Some(args) = shapes.iter().map(|shape| self.shape_type(shape, base.range.start)).collect::<Option<Vec<_>>>() else {
                        return Ok(None);
                    };
                    // The typed route admits only the flat payload-capable
                    // arguments for a generic enum; a struct or enum argument
                    // is a diagnostic, not an instantiation.
                    if let Some((index, _)) = args.iter().enumerate().find(|(_, ty)| !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF)) {
                        return Err(self.error_at(arguments[index].start, "type arguments must be i64, bool, f64, string, vec, or vec[string]"));
                    }
                    match self.instantiate_enum(gid, args, base.range.start) {
                        Ok(eid) => eid,
                        Err(_) => return Ok(None),
                    }
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
                self.seek_scalar_offset(expression.range.end);
                return Err(self.fail(&format!("variant '{}::{}' needs {} payload value(s)", def.name, vname, fields.len())));
            };
            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
            if !self.matches_value_type(*field, actual, Some(&argument.range)) {
                self.seek_scalar_offset(argument.range.end);
                return Err(self.fail(&format!("payload {} of '{}::{}' must be {}", index + 1, def.name, vname, field.name(self.ctx))));
            }
            // An owned payload value moves into the variant, exactly like an
            // owned struct field: the source slot's Frees is trimmed.
            if actual.owned()
                && let Expr::Variable(slot, ..) = &value {
                    self.check_variable_move(*slot)?;
                    self.moved.insert(*slot);
                }
            values.push(value);
        }
        self.seek_scalar_offset(expression.range.end);
        Ok(Some((Expr::EnumLit(def.name, eid, vidx, values), Type::Enum(def.name, eid))))
    }
}
