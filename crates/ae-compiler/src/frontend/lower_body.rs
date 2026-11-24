//! Lowering module: body (P1.4 parser split).
use super::*;
use super::route_body_stmt::*;
use super::route_marks::*;

impl<'a> Parser<'a> {
    pub(super) fn lower_resolved_scalar_body(&mut self) -> Option<Result<Vec<Statement>, ae_span::Diagnostic>> {
        self.lower_resolved_body_with_captures(&[])
    }

    pub(super) fn lower_resolved_body_with_captures(
        &mut self,
        capture_starts: &[(usize, usize)],
    ) -> Option<Result<Vec<Statement>, ae_span::Diagnostic>> {
        // Array parameters are borrowed views (`&[i64; N]` is the only array
        // parameter form, and `Type::Array` bakes the borrow in): their
        // element reads and stores flow through the same arms as local
        // arrays. File-local struct parameters join as borrowed field
        // receivers: `&P` reads its scalar fields, and `&mut P` also writes
        // them (a FieldStore, nothing moves through the borrow). By-value
        // struct parameters carry ownership effects (moves, drop at scope
        // exit) and stay typed.
        // Borrows of aggregates themselves (`&a`, `&p`) stay typed too — a
        // borrow occurrence never lands in `scalar_reads` — and so do calls
        // to aggregate-taking functions, which `supports_call` excludes.
        // Generic functions arrive here after the instance machinery has
        // substituted their signature. Keep non-scalar substitutions on
        // the typed route until their ownership and layout are covered.
        // A substitution may be any value type the body route handles:
// scalars and `string` both lower through the same arms (owned
// fields, concatenation, comparisons).
        // Every clause is named so `AE_ROUTE_REASONS=1` can say which one kept the
        // body off the route: the guard is the one rejection path with no
        // statement to point at.
        let subst_supported = self.subst.values().copied().all(|ty| {
            scalar_type(ty) || matches!(ty, Type::String | Type::Enum(..) | Type::Vec | Type::StrVec | Type::VecF | Type::Fn(_))
                || owned_plain_struct(ty, self.structs)
                || owned_field_struct(ty, self.structs)
                // A drop-carrying struct with plain fields as a substitution:
                // the instantiated body owns and releases it.
                || plain_drop_struct(ty, self.structs)
                || matches!(ty, Type::Struct(sid) if self.generics.closure_envs.contains_key(&sid))
        });
        let receiver_supported = !self.receiver.is_some_and(|ty| !matches!(ty, Type::Struct(_) | Type::StructRef(_) | Type::StructMut(_) | Type::Enum(..)));
        // A receiver-less impl body (an associated function, or a trait impl's
        // concrete method) routes like any function: a body that spells `Self`
        // in a way the marking pass cannot classify falls back at the scan.
        let impl_body_supported = true;
        let return_supported = scalar_type(self.return_type) || matches!(self.return_type, Type::Enum(..) | Type::String | Type::Fn(_))
            || matches!(self.return_type, Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF)
            || matches!(self.return_type, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
            || (matches!(self.return_type, Type::Struct(_))
                && (self.receiver == Some(self.return_type)
                    || owned_plain_struct(self.return_type, self.structs)
                    || owned_field_struct(self.return_type, self.structs)
                    || plain_drop_struct(self.return_type, self.structs)
                    || matches!(self.return_type, Type::Struct(sid) if self.generics.closure_envs.contains_key(&sid))));
        let slots_supported = self.names.iter().all(|(name, &(_, _, ty))| {
            // A value capture moves into the closure's slot; the body's
            // own Frees releases it after the call (the FnOnce shape),
            // and a string vector's Frees releases its elements in the
            // same order the typed path emits.
            // A `&mut` struct parameter is a borrowed receiver: its field
            // writes are FieldStore on a StructMut slot (the Checked arm
            // already lowers them), and nothing moves or drops through
            // the borrow.
            // A `string` *parameter* is owned by this body; `live_owned`
            // carries it in every `return`, so the callee releases what it
            // did not move out. A closure's string capture is the same owned
            // value (a value capture, the FnOnce shape): the exit Frees
            // releases it unless the body moved it out, and the move checks
            // reject a second read after a move.
            let ok = scalar_type(ty) || matches!(ty, Type::Array(_) | Type::StructRef(_) | Type::StructMut(_) | Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Vec | Type::VecF | Type::StrVec | Type::Map | Type::MapF | Type::Enum(..) | Type::Fn(_) | Type::Closure(_))
                || matches!(ty, Type::String)
                // A captured token cursor: the closure borrows the outer
                // cursor (the capture form is the cursor itself), and the
                // loop's borrow rules apply inside.
                || matches!(ty, Type::Tokens)
                || matches!(ty, Type::Ptr(id) | Type::PtrMut(id) if scalar_type(self.ctx.ptr_pointee(id)))
                || ty == Type::StrView
                // A handle is a copy-semantics value: passing one never
                // moves or releases anything, so a body whose locals are
                // `atomic`/`shared`/`mutex`/`chan`/`exec`/`pool` values
                // reads and passes them like scalars.
                || matches!(ty, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool | Type::Guard)
                // A thread handle (`spawn`) and a task scope (`scope_new`) are
                // owned values the body may bind, pass, and join.
                || matches!(ty, Type::Thread(_) | Type::TaskScope)
                // A by-value plain struct parameter is owned here, but it
                // carries nothing a release routine walks, so the callee
                // needs no exit drop: reads (and moving it back out) are
                // the whole surface.
                || owned_plain_struct(ty, self.structs)
                // A capture of a struct with owned fields is bound by value
                // in the closure's generated function; the caller's move
                // rules already made that capture a value.
                || owned_field_struct(ty, self.structs)
                // A drop-carrying struct with plain fields: the exit Frees
                // runs its generated release, which calls the user's drop.
                || plain_drop_struct(ty, self.structs)
                || matches!(ty, Type::Struct(sid) if self.generics.closure_envs.contains_key(&sid))
                || (name == "self" && self.receiver == Some(ty) && matches!(ty, Type::Struct(_) | Type::StructMut(_) | Type::Enum(..)));
            if !ok && std::env::var_os("AE_ROUTE_REASONS").is_some() {
                eprintln!("route reason blocked function: guard slot '{name}' of type {}", ty.name(self.ctx));
            }
            ok
        });
        if !(subst_supported && receiver_supported && impl_body_supported && return_supported && slots_supported) {
            if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                let mut failed = Vec::new();
                if !subst_supported { failed.push("substitution"); }
                if !receiver_supported { failed.push("receiver"); }
                if !impl_body_supported { failed.push("impl body"); }
                if !return_supported { failed.push("return type"); }
                if !slots_supported { failed.push("slot"); }
                eprintln!("route reason blocked function at {}: guard ({})",
                    self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), failed.join(", "));
            }
            return None;
        }
        let resolved_cell = Rc::clone(&self.resolved_expressions);
        let resolved = resolved_cell.get_or_init(|| self.build_resolved_expressions());
        let Some(body) = resolved.scalar_bodies.get(&self.tokens[self.pos].offset).cloned() else {
            if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                eprintln!("route blocked function at {}: not collected", self.tokens.get(self.pos).map(|t| t.offset).unwrap_or(0));
            }
            return None
        };
        let mut method_calls: HashMap<usize, bool> = HashMap::new();
        let mut local_structs: HashMap<String, Type> = HashMap::new();
        for statement in body.iter() {
            ae_syntax::statement::visit_statements(statement, &mut |statement| {
                if let ae_syntax::statement::StatementKind::Binding { name, initializer, .. } = &statement.kind {
                    if let ae_syntax::expression::ExpressionKind::Struct { target, .. } = &initializer.kind {
                        if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &target.kind
                            && let Some(&sid) = self.struct_names.get(struct_name) {
                                local_structs.insert(name.clone(), Type::Struct(sid));
                            }
                        // A generic struct literal names its instance's type.
                        if let ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments } = &target.kind
                            && let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &inner.kind
                                && let Some(&gid) = self.generics.struct_names.get(struct_name) {
                                    let args = self.shared_specialization_types(arguments).or_else(|| {
                                        let saved = self.pos;
                                        let mut parsed = Vec::with_capacity(arguments.len());
                                        for arg in arguments {
                                            self.seek_scalar_offset(arg.start);
                                            let Ok(ty) = self.named_type() else {
                                                self.pos = saved;
                                                return None;
                                            };
                                            parsed.push(ty);
                                        }
                                        self.pos = saved;
                                        Some(parsed)
                                    });
                                    if let Some(args) = args
                                        && let Ok(sid) = self.instantiate_struct(gid, args, target.range.start) {
                                            local_structs.insert(name.clone(), Type::Struct(sid));
                                        }
                                }
                    }
                    if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(other)) = &initializer.kind
                        && let Some(&ty) = local_structs.get(other).or_else(|| self.names.get(other).map(|(_, _, ty)| ty))
                    {
                        local_structs.insert(name.clone(), ty);
                    }
                    // A binding from a struct constructor call (`Point::new(..)`,
                    // `Pair[string, i64]::new(..)`): a layout instantiation only
                    // (no body parse), so it is safe in this precompute.
                    if let ae_syntax::expression::ExpressionKind::Call { callee, .. } = &initializer.kind
                        && let ae_syntax::expression::ExpressionKind::Qualified { base, .. } = &callee.kind {
                            match &base.kind {
                                ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments } => {
                                    if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &inner.kind
                                        && let Some(args) = self.shared_specialization_types(arguments) {
                                            if let Some(&gid) = self.generics.struct_names.get(struct_name) {
                                                if let Ok(sid) = self.instantiate_struct(gid, args, base.range.start) {
                                                    local_structs.insert(name.clone(), Type::Struct(sid));
                                                }
                                            } else if let Some(&gid) = self.generics.names.get(struct_name)
                                                && let Ok(eid) = self.instantiate_enum(gid, args, base.range.start) {
                                                    let display = self.enums[eid].name;
                                                    local_structs.insert(name.clone(), Type::Enum(display, eid));
                                                }
                                        }
                                }
                                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) => {
                                    if struct_name == "Self" {
                                        if let Some(impl_ty) = self.impl_self {
                                            local_structs.insert(name.clone(), impl_ty);
                                        }
                                    } else if let Some(&sid) = self.struct_names.get(struct_name) {
                                        local_structs.insert(name.clone(), Type::Struct(sid));
                                    } else if let Some(&eid) = self.enum_names.get(struct_name) {
                                        local_structs.insert(name.clone(), Type::Enum(self.enums[eid].name, eid));
                                    }
                                }
                                _ => {}
                            }
                        }
                    // A bare variant path (`let c = Opt[i64]::None;`) names
                    // the instance's enum type without opening a call.
                    if let ae_syntax::expression::ExpressionKind::Qualified { base, .. } = &initializer.kind {
                        let mut node = base.as_ref();
                        let mut arguments = None;
                        loop {
                            match &node.kind {
                                ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments: args }
                                | ae_syntax::expression::ExpressionKind::BracketedCallTarget { base: inner, arguments: args } => {
                                    arguments = self.shared_specialization_types(args);
                                    node = inner.as_ref();
                                }
                                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(type_name)) => {
                                    if type_name == "Self" {
                                        if let Some(impl_ty) = self.impl_self {
                                            local_structs.insert(name.clone(), impl_ty);
                                        }
                                    } else if let Some(&sid) = self.struct_names.get(type_name) {
                                        local_structs.insert(name.clone(), Type::Struct(sid));
                                    } else if let Some(&gid) = self.generics.names.get(type_name) {
                                        if let Some(args) = arguments
                                            && let Ok(eid) = self.instantiate_enum(gid, args, node.range.start) {
                                                let display = self.enums[eid].name;
                                                local_structs.insert(name.clone(), Type::Enum(display, eid));
                                            }
                                    } else if let Some(&eid) = self.enum_names.get(type_name) {
                                        local_structs.insert(name.clone(), Type::Enum(self.enums[eid].name, eid));
                                    }
                                    break;
                                }
                                _ => break,
                            }
                        }
                    }
                    // A binding from a method call whose signature returns a
                    // struct or enum (`let fixed = u2.with_min_radius(1);`,
                    // `let moved = start.shifted(1, 2).shifted(0, -6);`), or a
                    // pointer-valued binding (`let raw = ae_std_alloc(..)`,
                    // `let view = raw as *mut f64`, `let p = buf.offset(1)`):
                    // a parameter or an earlier local roots the chain, so each
                    // link's declared type answers statelessly and the gate can
                    // ask before the lowering declares the binding.
                    if let Some(ret @ (Type::Enum(..) | Type::Struct(_) | Type::Ptr(_) | Type::PtrMut(_)
                        | Type::Int(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Unit)) = self.chain_call_type(initializer, &local_structs)
                    {
                        local_structs.insert(name.clone(), ret);
                    } else if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(src)) = &initializer.kind
                        && let Some(&ty) = local_structs.get(src)
                    {
                        local_structs.insert(name.clone(), ty);
                    }
                }
                // A match payload binding of struct type (ADR-0234): the arm
                // reads fields through the binding before the lowering
                // declares it, so the prepass types it from the variant's
                // payload shape.
                if let ae_syntax::statement::StatementKind::Match { arms, .. } = &statement.kind {
                    for arm in arms {
                        let ae_syntax::pattern::PatternKind::Variant { enum_name, variant, payload: Some(bindings), .. } = &arm.pattern.kind else { continue };
                        let Some(&eid) = self.enum_names.get(enum_name.as_str()) else { continue };
                        let Some((_, types)) = self.enums[eid].variants.iter().find(|(name, _)| name == variant) else { continue };
                        for (binding, ty) in bindings.iter().zip(types.iter()) {
                            if let ae_syntax::pattern::PatternKind::Binding(name) = &binding.kind
                                && matches!(ty, Type::Struct(_) | Type::Enum(..)) {
                                local_structs.insert(name.clone(), *ty);
                            }
                        }
                    }
                }
                for root in statement.expressions() {
                    visit_expression_tree(root, &mut |expression| {
                        let ae_syntax::expression::ExpressionKind::Call { callee, arguments } = &expression.kind else { return };
                        let ae_syntax::expression::ExpressionKind::Field { base, name } = &callee.kind else { return };
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind else { return };
                        // A pointer receiver: the resolver's spelling mark names
                        // the binding before the lowering declares it, so the
                        // verdict does not depend on the slot table.
                        if resolved.scalar_pointer_receivers.contains(&base.range.start) {
                            let ok = match name.as_str() {
                                "deref" => arguments.is_empty(),
                                "offset" | "byte_offset" => arguments.len() == 1,
                                // Any other name reaches the lowering's own
                                // `pointers have no method '...'`, the typed
                                // route's diagnostic.
                                _ => true,
                            };
                            method_calls.insert(callee.range.start, ok);
                            return;
                        }
                        let ty = self.names.get(receiver).map(|&(_, _, ty)| ty)
                            .or_else(|| local_structs.get(receiver).copied());
                        let Some(ty) = ty else { return };
                        let ok = if matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                            match name.as_str() {
                                "deref" => arguments.is_empty(),
                                "offset" | "byte_offset" => arguments.len() == 1,
                                // The lowering answers any other name with the
                                // pointer's own diagnostic.
                                _ => true,
                            }
                        } else if let Some((signature, _, _)) = self.resolve_inherent_method(name, ty) {
                            // Arity is part of the gate: a mismatched call
                            // falls back instead of reaching the lowering.
                            arguments.len() + 1 == signature.params.len()
                        } else {
                            // A method the table knows but the route's receiver
                            // rules refuse (an associated function called
                            // through a value): the lowering's own diagnostic
                            // answers, so the gate admits it.
                            self.inherent_entry_exists(name, ty)
                        };
                        method_calls.insert(callee.range.start, ok);
                    });
                }
            });
        }
        let supports_call = |name: &str, offset: usize| {
            // The pure value builtins lower natively (lower_resolved_builtin);
            // every other reserved name keeps the typed body.
            if resolved_builtin(name) { return true; }
            // A generic function: the spelled form (`identity[i64](41)`)
            // instantiates through the bracketed arm, and a bare call reaches
            // the lowering's own "write its type arguments" diagnostic.
            if self.generics.fn_names.contains_key(name) {
                return true;
            }
            // A trait method of one of this function's constrained type
            // parameters: the instance's substitution picks the impl, so the
            // call lowers as a plain (monomorphized) call.
            if self.trait_bounds.values().flatten().any(|&tid| {
                self.generics.traits.get(tid).is_some_and(|defn| defn.methods.iter().any(|method| method.name == name))
            }) {
                return true;
            }
            !RESERVED.contains(&name) && self.functions.get(name).is_some_and(|signature| {
                // File-local resolution leaves imports open. The compiler's
                // registry already contains the signatures of loaded modules.
                let visible = if signature.file == self.file {
                    resolved.direct_calls.contains(&offset)
                } else {
                    // A private import is admitted too: the lowering reports
                    // the typed route's "private to <file>" diagnostic.
                    signature.file != PRELUDE && resolved.unresolved_calls.contains(&offset)
                };
                visible
                    // Calling C, or an `unsafe fn`, is the unchecked
                    // operation: the scan admits the shape and the lowering
                    // enforces the `unsafe` block (so the pinned E0701 travels
                    // the same path).
                    && (scalar_type(signature.ret) || owned_plain_struct(signature.ret, self.structs)
                        || matches!(signature.ret, Type::String | Type::Enum(..) | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF | Type::Fn(_))
                        || matches!(signature.ret, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
                        // A scalar-pointee pointer (`extern "C" fn
                        // ae_std_alloc(..) -> *mut i64`): the binding holds a
                        // copy-semantics handle the pointer ops consume, the
                        // same shape the slot guard admits.
                        || matches!(signature.ret, Type::Ptr(id) | Type::PtrMut(id) if scalar_type(self.ctx.ptr_pointee(id)))
                        || owned_field_struct(signature.ret, self.structs)
                        || plain_drop_struct(signature.ret, self.structs)
                        || matches!(signature.ret, Type::Struct(sid) if self.generics.closure_envs.contains_key(&sid)))
                    && signature.params.iter().copied().all(|ty| {
                        // Owned-aggregate parameters join the route: the call
                        // moves the argument, tracked in `moved`, and the
                        // drops pass trims the Frees lists. A `&string`
                        // parameter (the resolver's `StrView`) is a read-only
                        // view: the argument lowers as `Argument::View`. The
                        // other borrows take the argument's address
                        // (`Argument::Borrow`) with the typed route's own
                        // classify rules.
                        scalar_type(ty) || matches!(ty, Type::Struct(_) | Type::String | Type::StrView | Type::Array(_)
                            | Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::StructRef(_) | Type::StructMut(_)
                            // A by-value vector or map parameter owns the
                            // argument: the call moves it (tracked in
                            // `moved`).
                            | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF
                            // An enum parameter owns a value argument the
                            // same way (a result enum, an Option payload).
                            | Type::Enum(..)
                            // A function-typed parameter takes a closure
                            // literal or a function name (`Argument::FnPtr`).
                            | Type::Fn(_)
                            // A raw-pointer parameter (`extern "C" fn
                            // ae_std_free(block: *mut i64)`): a pointer is a
                            // copy-semantics handle, so the argument passes by
                            // value with the typed route's exact type check.
                            | Type::Ptr(_) | Type::PtrMut(_)
                            // A guard is an owned handle a callee may take by
                            // value (`consume(g)`), the same move rule as the
                            // other handles.
                            | Type::Guard
                            | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
                    })
            })
        };
        // Slots first: the field gate below answers by slot type, not by the
        // impl receiver.
        let mut slots = HashMap::new();
        for &(slot, _, ty) in self.names.values() {
            let start = self.binding_ranges.get(&slot).map(|range| range.start)
                .or_else(|| capture_starts.iter().find_map(|&(capture, start)| (capture == slot).then_some(start)));
            // These two lookups used to `?` out silently: a body whose slots
            // cannot be mapped fell back with no explanation at all. Name the
            // cause when the reason report is on.
            let Some(start) = start else {
                if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                    eprintln!("route reason blocked function at {}: slot {slot} ({}) has no binding range",
                        self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), ty.name(self.ctx));
                }
                return None;
            };
            let Some(&def) = resolved.scalar_definitions.get(&start) else {
                if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                    eprintln!("route reason blocked function at {}: slot {slot} ({}) start {start} is not an indexed binding",
                        self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), ty.name(self.ctx));
                }
                return None;
            };
            slots.insert(def, (slot, ty));
        }
        // Method-call verdicts, precomputed with `&self` so the gate closure below
// captures only this map. A receiver is a parameter/`self` (known through
// `names` already) or a local bound to a struct construction; anything
// else — generic-instance receivers included — is rejected here and the
// body falls back whole.
        // Any routed struct slot can root a field chain: the root ident's
        // occurrence names the def, the def names the slot, and the slot's
        // type drives the field walk (a `&mut` struct parameter roots its
        // writes the same way `self` does). Reads and stores lower through
        // the same slot lookups the impl path uses.
        let supports_field = |expression: &ae_syntax::expression::Expression, role| {
            // A match subject: `match self` is valid when this body's
            // receiver is an enum (by-value dispatch or a generic instance).
            if role == ae_resolve::Role::Borrow {
                return matches!(&expression.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) if name == "self")
                    && (matches!(self.receiver, Some(Type::Enum(..))) || matches!(self.impl_self, Some(Type::Enum(..))));
            }
            // A method-call callee: the precomputed verdict admits the
            // inherent cases; a struct/enum receiver is admitted permissively
            // and the lowering resolves the generic instance lazily (it owns
            // `&mut self` and emits the typed diagnostics when the method is
            // missing), so the gate never depends on instantiation order.
            if role == ae_resolve::Role::Call {
                if method_calls.get(&expression.range.start).copied().unwrap_or(false) {
                    return true;
                }
                let ae_syntax::expression::ExpressionKind::Field { base, name } = &expression.kind else { return false };
                let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind else {
                    // A temporary or chained receiver: the lowering delegates
                    // the whole call to the typed postfix layer.
                    return matches!(base.kind, ae_syntax::expression::ExpressionKind::Call { .. }
                        | ae_syntax::expression::ExpressionKind::Group(_));
                };
                // A body local that the precompute typed (a struct literal or
                // constructor call) answers here even though the lowering has
                // not declared it yet.
                let ty = self.names.get(receiver).map(|&(_, _, ty)| ty)
                    .or_else(|| local_structs.get(receiver).copied());
                if ty.is_none()
                    && (resolved.scalar_reads.get(&base.range.start)
                            .or_else(|| resolved.call_definitions.get(&base.range.start))
                            .is_some_and(|id| resolved.generic_call_bindings.contains(id)
                                || resolved.owned_struct_bindings.contains(id)))
                {
                    // A local from an explicit generic call or inferred generic
                    // struct: the lowering has its concrete type once instantiated.
                    return true;
                }
                let Some(ty) = ty else { return false };
                if matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                    return matches!(name.as_str(), "deref" | "offset" | "byte_offset");
                }
                return matches!(ty, Type::Struct(_) | Type::StructMut(_) | Type::StructRef(_) | Type::Enum(..));
            }
            let Some(root) = field_chain_root(expression) else { return false };
            let def = match role {
                ae_resolve::Role::Read => resolved.scalar_reads.get(&root.range.start)
                    .or_else(|| resolved.call_definitions.get(&root.range.start)),
                ae_resolve::Role::Write => resolved.scalar_writes.get(&root.range.start),
                _ => return false,
            };
            let Some(def) = def else { return false };
            let Some(&(_, ty)) = slots.get(def) else {
                // A body local declared after the entry scope (`var plot = ..;
                // plot.w = 40;`): the lowering owns its slot and validates the
                // write there. A local from an explicit generic call is the
                // same shape for reads — its type exists only at lowering.
                return match role {
                    ae_resolve::Role::Write => resolved.struct_field_writes.contains(&expression.range.start)
                        || resolved.owned_field_writes.contains(&expression.range.start)
                        // A body local declared after the entry scope (a
                        // generic-instance binding, `var p = Pair[..] { .. }`):
                        // the lowering's FieldStore arm validates the receiver
                        // type, the field, and the binding's mutability.
                        || resolved.scalar_writes.contains_key(&root.range.start),
                    ae_resolve::Role::Read => resolved.generic_call_bindings.contains(def)
                        || matches!(&root.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                            if local_structs.get(name).is_some_and(|&ty| !field_chain_is_bare(expression)
                                && receiver_field_type(expression, ty, self.structs)
                                    .is_some_and(|field| scalar_type(field)
                                        // A string field reads as a borrowed
                                        // view (print, a borrow argument, a
                                        // comparison); binding it as a value
                                        // is the binding arm's own move check.
                                        || matches!(field, Type::Ptr(_) | Type::PtrMut(_) | Type::Fn(_) | Type::String)
                                        || matches!(field, Type::Struct(env_sid) if self.generics.closure_envs.contains_key(&env_sid))))),
                    _ => false,
                };
            };
            if role == ae_resolve::Role::Write && !matches!(ty, Type::StructMut(_) | Type::Struct(_) | Type::StructRef(_)) {
                return false;
            }
            if field_chain_is_bare(expression) {
                // The whole value: only an owned struct moves as one.
                matches!(ty, Type::Struct(_))
            } else {
                // A string field joins the write surface (the FieldStore
                // codegen releases the old value and consumes the new one);
                // reads stay scalar-only here and reach the scan through the
                // view and read marks.
                receiver_field_type(expression, ty, self.structs)
                    .is_some_and(|field| scalar_type(field)
                        // A raw pointer field reads as its address (a copyable
                        // value with no release surface).
                        || (role == ae_resolve::Role::Read && matches!(field, Type::Ptr(_) | Type::PtrMut(_)))
                        // A string field reads as a borrowed view (print, a
                        // borrow argument, a comparison); binding it as a value
                        // is the binding arm's own move rejection.
                        || (role == ae_resolve::Role::Read && matches!(field, Type::String))
                        || (role == ae_resolve::Role::Write
                            && matches!(field, Type::String | Type::Vec | Type::VecF | Type::StrVec | Type::Map | Type::MapF)))
            }
        };
        // Parameters owned by this body, minus capture slots (the closure
        // lowering already accounts for those): `live_owned` releases them at
        // every return unless the body moved one out.
        self.owned_params = self.names.values()
            .filter(|&&(slot, _, ty)| self.is_owned(ty) && !capture_starts.iter().any(|&(capture, _)| capture == slot))
            .map(|&(slot, _, ty)| (slot, ty))
            .collect();
        if let Some(statement) = body.iter().position(|statement| {
                !scalar_body_statement(statement, resolved, &supports_call, &supports_field)
            }).and_then(|index| body.get(index)) {
            // The body was collected, so the scan admitted it; this statement
            // fails the lowering's own gates (callee signatures, slot types).
            self.report_route_reason("blocked", statement, &|s| !scalar_body_statement(s, resolved, &supports_call, &supports_field));
            self.owned_params.clear();
            return None;
        }
        let lowered = self.lower_scalar_statements(&body, &mut slots);
        self.owned_params.clear();
        Some(lowered)
    }

    pub(super) fn lower_scalar_statements(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        use ae_syntax::statement::StatementKind;
        self.take(Kind::LBrace)?;
        if self.block_depth >= 64 { return Err(self.fail("block nesting limit exceeded (64)")); }
        self.block_depth += 1;
        self.scopes.push(Vec::new());
        let mut statements = Vec::with_capacity(body.len());
        let lowered: Result<(), ae_span::Diagnostic> = (|| {
            for statement in body {
                // A call argument's borrow covered only the statement that
                // made the call; the next statement may borrow or write the
                // binding again (the typed route's block loop does the same).
                self.call_borrows.clear();
                self.mut_borrows.clear();
                if matches!(statement.kind, StatementKind::ForRange { .. }) {
                    self.lower_scalar_range(statement, slots, &mut statements)?;
                } else if matches!(statement.kind, StatementKind::ForIn { .. }) {
                    self.lower_scalar_for_in(statement, slots, &mut statements)?;
                } else {
                    let statement = self.lower_scalar_statement(statement, slots)?;
                    if matches!(statement, Statement::Break | Statement::Continue(_)) {
                        // Owned values declared inside the loop body die at
                        // the jump; releases are idempotent.
                        let owned = self.loop_local_owned();
                        if !owned.is_empty() { statements.push(Statement::Frees(owned)); }
                    }
                    statements.push(statement);
                }
            }
            Ok(())
        })();
        let scope = self.scopes.pop().unwrap();
        self.block_depth -= 1;
        lowered?;
        self.take(Kind::RBrace)?;
        for statement in body {
            if let StatementKind::Binding { name, name_range, .. } = &statement.kind {
                self.names.remove(name);
                if let Some(&def) = self.resolved_expressions().scalar_definitions.get(&name_range.start) { slots.remove(&def); }
            }
        }
        let depth = self.scopes.len();
        self.borrows.retain(|&(_, borrow_depth, _)| borrow_depth <= depth);
        // Owned values declared in this block are released at its end,
        // mirroring the typed route's block exit.
        let owned: Vec<(usize, Type)> = scope.iter().rev().filter(|(_, ty)| self.is_owned(*ty)).copied().collect();
        if !owned.is_empty() { statements.push(Statement::Frees(owned)); }
        Ok(statements)
    }

    pub(super) fn seek_scalar_offset(&mut self, offset: usize) {
        self.pos = self.tokens.partition_point(|token| token.offset < offset);
    }

}
