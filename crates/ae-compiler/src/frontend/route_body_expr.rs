//! Expression admissibility for the resolved body route.
use super::*;
use super::route_marks::*;

pub(super) fn scalar_body_expression(expression: &ae_syntax::expression::Expression, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool, supports_field: &impl Fn(&ae_syntax::expression::Expression, ae_resolve::Role) -> bool) -> bool {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        // A name no local answers: the lowering reports the typed route's
        // exact diagnostic (`unknown variable`, E0101, or the struct-name
        // remedy) at the name itself, so the scan admits it. The marked
        // shapes keep their exclusion rules.
        ExpressionKind::Atom(Kind::Ident(name)) if name != "self" => match resolved.scalar_reads.get(&expression.range.start) {
            Some(id) => !resolved.array_definitions.contains(id) && !resolved.struct_definitions.contains(id)
                && !resolved.owned_vectors.contains(id) && !resolved.owned_text_bindings.contains(id)
                && !resolved.enum_bindings.contains(id)
                && !resolved.token_cursor_definitions.contains(id)
                || (resolved.owned_text_bindings.contains(id)
                    && resolved.closure_capture_reads.contains(&expression.range.start))
                // A struct borrow read as a value: the binding arm's own
                // "cannot store a borrowed struct" diagnostic answers.
                || resolved.struct_borrow_reads.contains(id),
            None => true,
        },
        // A construction of a file-local struct: the drop-free scalar
        // shape lowers as a value, and the scalar-or-`string` shape owns its
        // string field values (a literal, a moved owned string, or an
        // expression the scalar arm already admits such as a concat or a
        // `to_string` call). Field types are checked when the literal lowers.
        ExpressionKind::Struct { target, fields } => {
            let marked = resolved.drop_free_constructions.contains(&expression.range.start)
                || resolved.owned_constructions.contains(&expression.range.start)
                // A struct that declares `drop`: the value is owned, and
                // every exit path releases it through the generated routine,
                // which calls the user's `drop` first.
                || resolved.drop_constructions.contains(&expression.range.start);
            // A specialization (`Cell[T] { .. }`) is permissive here: the
            // instance's field types exist only at lowering time and are
            // checked there.
            let specializes = matches!(&target.kind, ExpressionKind::Specialization { base, .. }
                if matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))));
            let named = matches!(target.kind, ExpressionKind::Atom(Kind::Ident(_)));
            (named || specializes)
                && (marked || specializes)
                && fields.iter().all(|field| {
                    scalar_body_expression(&field.value, resolved, supports_call, supports_field)
                        || matches!(field.value.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || matches!(&field.value.kind, ExpressionKind::Closure { body, .. }
                            if resolved.scalar_bodies.contains_key(&body.start))
                        || matches!(&field.value.kind, ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&field.value.range.start)
                                .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    // An owned struct or enum value moves into
                                    // the field (`SocketAddrV4 { ip: ip, .. }`).
                                    || resolved.struct_definitions.contains(id)
                                    || resolved.owned_struct_bindings.contains(id)
                                    || resolved.enum_bindings.contains(id)))
                })
        }
        // A struct parameter's field read, possibly nested (`o.inner.x`):
        // the chain's base may be a scalar read or a struct-typed chain
        // receiver, but only a scalar field lowers as a value.
        ExpressionKind::Field { base, .. } => {
            if receiver_field_root(expression).is_some() {
                return supports_field(expression, ae_resolve::Role::Read);
            }
            // A field read on a *plain* call result (`f().x`,
            // `compute_mag[Delta](d).offset`): the call must be admissible and
            // the lowering checks the return type is a struct. A field on a
            // method-call result (`p.new(2).x`) is admitted permissively: the
            // method resolution's pinned diagnostics (an associated function
            // called through a value, a missing method) are the lowering's.
            let plain_call_base = match &base.kind {
                ExpressionKind::Call { callee, .. } => {
                    matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(_)) | ExpressionKind::Qualified { .. })
                        || matches!(&callee.kind, ExpressionKind::BracketedCallTarget { base: inner, .. }
                            if matches!(inner.kind, ExpressionKind::Atom(Kind::Ident(_))))
                }
                _ => false,
            };
            let method_call_base = match &base.kind {
                ExpressionKind::Call { callee, .. } => {
                    matches!(&callee.kind, ExpressionKind::Field { base: inner, .. }
                        if matches!(inner.kind, ExpressionKind::Atom(Kind::Ident(_))))
                }
                _ => false,
            };
            let rooted = match &base.kind {
                ExpressionKind::Atom(Kind::Ident(_)) => true,
                ExpressionKind::Field { .. } => resolved.struct_field_reads.contains(&base.range.start)
                    || resolved.struct_field_chains.contains(&base.range.start),
                ExpressionKind::Call { .. } => (plain_call_base || method_call_base)
                    && scalar_body_expression(base, resolved, supports_call, supports_field),
                _ => false,
            };
            rooted && (resolved.struct_field_reads.contains(&expression.range.start)
                || plain_call_base
                // The lowering's method resolution answers first; a resolved
                // method call's struct result then reads the field.
                || method_call_base
                // A local from an explicit generic call: the instantiated
                // return type exists only at lowering time.
                || matches!(&base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && resolved.scalar_reads.get(&base.range.start)
                        .is_some_and(|id| resolved.generic_call_bindings.contains(id))
                // A struct-typed parameter or local whose spelling the resolver
                // leaves open (a generic instance, an imported type): the scan
                // is permissive for an ident-rooted chain and the lowering's
                // re-check answers from the slot table.
                || supports_field(expression, ae_resolve::Role::Read))
        }
        // A name with a bracket that the syntax tree kept as a specialization
        // (`arr[j]` at the end of an expression): the typed expression parser
        // disambiguates it against the known names and builds the index.
        ExpressionKind::Specialization { base, arguments } => {
            matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && arguments.len() == 1
                && resolved.scalar_reads.get(&base.range.start).is_some_and(|id| {
                    resolved.array_definitions.contains(id) || resolved.vector_definitions.contains(id)
                })
        }
        ExpressionKind::Index { base, index } => {
            matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.get(&base.range.start).is_some_and(|id| {
                    resolved.array_definitions.contains(id)
                        // A vector indexes as element copies/views, whether
                        // the binding owns the vector or borrows it
                        // (`values: &vec`); the vector itself never moves.
                        || resolved.vector_definitions.contains(id)
                })
                // A closure parameter's `[i64; N]` (or any local whose array
                // shape the marks miss) indexes permissively: the lowering
                // answers by the slot's type.
                .then_some(())
                .or_else(|| resolved.scalar_reads.get(&base.range.start).map(|_| ()))
                .is_some()
                && scalar_body_expression(index, resolved, supports_call, supports_field)
        }
        ExpressionKind::Atom(Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False | Kind::Text(_) | Kind::Char(_)) | ExpressionKind::Unit => true,
        // A collection literal used as a value (`print(vec[1])`): the scan
        // admits it when every element does, and the lowering builds the same
        // `Expr::VecLit`/array the typed route builds. The print arm's type
        // check then rejects a vector exactly as the typed route does.
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            elements.iter().all(|element| scalar_body_expression(element, resolved, supports_call, supports_field))
        }
        ExpressionKind::AddressCast { binding_range, .. } => {
            resolved.scalar_address_casts.contains(&expression.range.start)
                && resolved.scalar_reads.contains_key(&binding_range.start)
        }
        // A pointer reinterpretation (`raw as *mut f64`): the base is a
        // pointer value and the pointee spelling is the typed parser's
        // business. Nothing moves — a pointer is a copy-semantics handle.
        ExpressionKind::PointerCast { base, .. } => {
            scalar_body_expression(base, resolved, supports_call, supports_field)
        }
        // A scalar `as` cast reads its base and names a scalar type; the
        // lowering owns the conversion rules.
        ExpressionKind::ScalarCast { base, .. } => {
            scalar_body_expression(base, resolved, supports_call, supports_field)
        }
        ExpressionKind::Group(inner) | ExpressionKind::Prefix { operand: inner, .. } => {
            scalar_body_expression(inner, resolved, supports_call, supports_field)
        }
        ExpressionKind::Binary { left, right, operator } => {
            // A string-typed field reads as a borrowed view inside a `+`
            // (the typed route's plus-operand rule): the field itself never
            // moves, so the concat may use it even though a bare read of the
            // field is not a value. An owned string variable is consumed by
            // the same `+`, so it is admissible here too.
            let view = |expression: &ae_syntax::expression::Expression| {
                if resolved.struct_field_views.contains(&expression.range.start) {
                    // A borrowed string field reads by `+` and by the
                    // equality/comparison operators; it never moves.
                    return *operator == Kind::Plus
                        || matches!(operator, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq);
                }
                // A comparison only reads the string (the typed route's
                // equality does not consume); every other operator keeps the
                // `+`-only rule.
                if *operator != Kind::Plus
                    && !matches!(operator, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq)
                {
                    return false;
                }
                matches!(&expression.kind, ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&expression.range.start)
                        .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)))
            };
            // A known local operand is admitted permissively: the infix
            // lowering type-checks it (a vector in `v + 1.0` reports the typed
            // route's mixed i64/f64 arithmetic, a struct its own error).
            let local_read = |expression: &ae_syntax::expression::Expression| {
                matches!(&expression.kind, ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.contains_key(&expression.range.start))
            };
            (scalar_body_expression(left, resolved, supports_call, supports_field) || view(left) || local_read(left))
                && (scalar_body_expression(right, resolved, supports_call, supports_field) || view(right) || local_read(right))
        }
        ExpressionKind::Call { callee, arguments } => {
            // A call whose name no declaration answers: the typed route errors
            // at the callee before any argument or return-type question, and
            // the lowering reproduces that E0207 — so the scan admits the
            // shape.
            if let ExpressionKind::Atom(Kind::Ident(_)) = &callee.kind
                && resolved.unknown_calls.contains(&callee.range.start) {
                return true;
            }
            if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                && typed_intrinsic(name) {
                return true;
            }
            // `len` of a bare fixed array is a compile-time constant and of a
            // vector or map a borrow of the slot: the builtin's fast path
            // reads the binding's definition, not a value, so the scan admits
            // the name even where the value-position rules exclude it.
            if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                && name == "len"
                && let [argument] = arguments.as_slice()
                && matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.contains_key(&argument.range.start) {
                return true;
            }
            // An associated call (`Point::new(..)`, `Type[..]::new(..)`): the
            // typed postfix layer owns instance construction, arity, and the
            // pinned associated-function diagnostics; the lowering delegates
            // the whole call.
            if matches!(&callee.kind, ExpressionKind::Qualified { .. }) {
                return true;
            }
            if let ExpressionKind::Field { base, name } = &callee.kind {
                if IntMethodKind::from_name(name).is_some()
                    && scalar_body_expression(base, resolved, supports_call, supports_field)
                    && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call, supports_field))
                {
                    return true;
                }
                if name == "deref" && arguments.is_empty()
                    && matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && resolved.scalar_pointer_receivers.contains(&base.range.start)
                {
                    return true;
                }
                // A method call: the gate answers `Role::Call` for the callee
                // node. The scan is permissive (a bare-ident receiver with
                // admissible arguments); the lowering's re-check resolves the
                // method and rejects the rest, which falls the body back to
                // the typed route.
                return (supports_field(callee, ae_resolve::Role::Call) || supports_field(callee, ae_resolve::Role::Read))
                    && arguments.iter().all(|argument| {
                        scalar_body_expression(argument, resolved, supports_call, supports_field)
                            || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&argument.range.start)
                            || resolved.struct_field_handles.contains(&argument.range.start)
                            || (matches!(argument.kind, ExpressionKind::Field { .. })
                                && supports_field(argument, ae_resolve::Role::Read))
                            || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id)
                                        || resolved.owned_vectors.contains(id)
                                        || resolved.map_definitions.contains(id)))
                            // An enum construction as a method argument.
                            || resolved.enum_constructors.contains(&argument.range.start)
                            // An enum value passed by value to a method.
                            || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                    .is_some_and(|id| resolved.enum_bindings.contains(id)))
                            // A closure literal passed to a method.
                            || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                                if resolved.scalar_bodies.contains_key(&body.start))
                            // A borrow argument: the typed postfix layer
                            // classifies it (and rejects an alias), so the
                            // lowering delegates a method call that carries one.
                            || matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                    });
            }
            // A call through a function-typed value: the binding names the
            // pointer the caller passed (`Expr::FnCall` on its slot).
            if matches!(callee.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.call_definitions.get(&callee.range.start)
                    .is_some_and(|id| resolved.fn_value_bindings.contains(id)
                        || (!resolved.direct_calls.contains(&callee.range.start)
                            && !resolved.unknown_calls.contains(&callee.range.start)
                            && !resolved.unresolved_calls.contains(&callee.range.start)))
            {
                return arguments.iter().all(|argument| {
                    scalar_body_expression(argument, resolved, supports_call, supports_field)
                        || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || resolved.struct_field_views.contains(&argument.range.start)
                        || resolved.struct_field_handles.contains(&argument.range.start)
                        // A borrow argument: the typed parser classifies it
                        // (and rejects an alias), so the lowering delegates a
                        // call that carries one whole.
                        || matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                });
            }
            // A call on a closure value: the typed machinery reads the captures
            // at the call site; the scan admits the shape and the lowering
            // validates the callee and the arguments.
            if let ExpressionKind::Atom(Kind::Ident(_)) = &callee.kind
                && resolved.call_definitions.get(&callee.range.start)
                    .is_some_and(|id| resolved.closure_bindings.contains(id))
                {
                    return arguments.iter().all(|argument| {
                        scalar_body_expression(argument, resolved, supports_call, supports_field)
                            || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&argument.range.start)
                            || resolved.struct_field_handles.contains(&argument.range.start)
                    });
                }
            // An explicit generic call: `identity[i64](41)`. The scan admits
            // the shape permissively; the lowering instantiates the function
            // and re-checks the substituted argument list.
            if let ExpressionKind::BracketedCallTarget { base, .. } = &callee.kind {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return false };
                return supports_call(name, base.range.start)
                    && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call, supports_field)
                        || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || resolved.struct_field_views.contains(&argument.range.start)
                        || resolved.struct_field_handles.contains(&argument.range.start)
                        || resolved.struct_field_reads.contains(&argument.range.start)
                        || (matches!(argument.kind, ExpressionKind::Field { .. })
                            && supports_field(argument, ae_resolve::Role::Read))
                        || resolved.enum_constructors.contains(&argument.range.start)
                        || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                            if resolved.scalar_bodies.contains_key(&body.start))
                        // An owned aggregate passed by value moves into the
                        // instance's substituted parameter.
                        || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                .is_some_and(|id| resolved.struct_definitions.contains(id)
                                    || resolved.enum_bindings.contains(id)
                                    || resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    || resolved.owned_vectors.contains(id)
                                    || resolved.map_definitions.contains(id)
                                    || resolved.owned_struct_bindings.contains(id)))
                        || matches!(argument.kind, ExpressionKind::BorrowArgument { .. }));
            }
            // A call on an arbitrary expression yielding `fn(...) -> R`, such
            // as `(h.cb)(x)` or `pick(true)(x)`.
            if !matches!(
                callee.kind,
                ExpressionKind::Atom(Kind::Ident(_))
                    | ExpressionKind::Qualified { .. }
                    | ExpressionKind::Field { .. }
                    | ExpressionKind::BracketedCallTarget { .. }
            ) && scalar_body_expression(callee, resolved, supports_call, supports_field)
                && arguments.iter().all(|argument| {
                    scalar_body_expression(argument, resolved, supports_call, supports_field)
                        || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || resolved.struct_field_views.contains(&argument.range.start)
                        || resolved.struct_field_handles.contains(&argument.range.start)
                        || matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                })
            {
                return true;
            }
            // The argument list below needs the callee's identity; a `matches!`
            // guard's bindings do not escape the macro, so the builtin answer is
            // taken once here.
            let callee_builtin = matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if resolved_builtin(name));
            matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name))
                if (resolved.direct_calls.contains(&callee.range.start)
                    || resolved.unresolved_calls.contains(&callee.range.start))
                    && supports_call(name, callee.range.start))
                && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call, supports_field)
                    || receiver_value(argument).is_some_and(|atom| supports_field(atom, ae_resolve::Role::Read))
                    // An owned aggregate passed by value is a move, not a
                    // value read: the ident eligibility excludes struct
                    // slots, so the argument position re-admits them.
                    || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                            .is_some_and(|id| resolved.struct_definitions.contains(id)
                                // An enum value passed by value (`describe(a)`):
                                // the call moves it.
                                || resolved.enum_bindings.contains(id)))
                    // A string-typed field in argument position reads as its
                    // view (builtins and `&string` parameters only ever
                    // borrow it).
                    || resolved.struct_field_views.contains(&argument.range.start)
                    || resolved.struct_field_handles.contains(&argument.range.start)
                    // A scalar field read as an argument (`describe(id.left)`):
                    // scalars copy, nothing moves. The scan is permissive for
                    // an ident-rooted chain; the lowering validates the field
                    // type (view, scalar, or rejected struct chain).
                    || resolved.struct_field_reads.contains(&argument.range.start)
                    || (matches!(argument.kind, ExpressionKind::Field { .. })
                        && supports_field(argument, ae_resolve::Role::Read))
                    // An owned string in argument position: a `&string`
                    // parameter views it, a `string` parameter moves it (the
                    // lowering records which).
                    || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                            .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                || resolved.owned_string_bindings.contains(id)
                                // An owned vector in argument position: a
                                // builtin (`len`) or a `&vec` parameter only
                                // borrows it. A map is borrowed the same way.
                                || resolved.owned_vectors.contains(id)
                                || resolved.map_definitions.contains(id)))
                    || matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                    // An enum construction passed directly (`f(Shape::Circle(3))`).
                    || resolved.enum_constructors.contains(&argument.range.start)
                    // A builtin over a struct field (`map_inc(b.counts, ..)`,
                    // `len(b.counts)`): the builtin's own arm type-checks the
                    // field, so the scan admits the read permissively.
                    || (callee_builtin && matches!(argument.kind, ExpressionKind::Field { .. }))
                    // A closure literal argument (`apply(|x: i64| -> i64
                    // {..}, 21)`) whose body the route collected.
                    || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                        if resolved.scalar_bodies.contains_key(&body.start)))
        }
        ExpressionKind::Closure { body, .. } if resolved.scalar_bodies.contains_key(&body.start) => true,
        _ => false,
    }
}
