//! Lowering module: call (P1.4 parser split).
use super::*;
use super::parser::moved_value_hint;
use super::route_marks::*;

impl<'a> Parser<'a> {
    /// The spawn statement forms (`task_spawn(exec, || ..)`,
    /// `scope_spawn(sc, || ..)`, `pool_spawn(pool, || ..)`): the handle first,
    /// then the callable whose captures move into the task. Shared by the
    /// typed statement parser and the resolved route; the cursor sits on the
    /// name.
    pub(super) fn spawn_statement(
        &mut self,
        name: &str,
        call: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let ExpressionKind::Call { arguments, .. } = &call.kind else {
            unreachable!("the spawn statement form is a call")
        };
        self.seek_scalar_offset(call.range.start);
        let (line, col) = self.position();
        let at = self.tokens[self.pos].offset;
        self.pos += 2; // the name and '('
        let Some(first) = arguments.first() else {
            self.seek_scalar_offset(call.range.end);
            self.pos = self.pos.saturating_sub(1);
            return Err(self.fail("expected expression").with_code("E0011"));
        };
        self.seek_scalar_offset(first.range.start);
        let (ex, ex_ty) = self.lower_scalar_body_expression(first, slots)?;
        self.seek_scalar_offset(first.range.end);
        let expected_ty = match name {
            "task_spawn" => Type::Exec,
            "scope_spawn" => Type::TaskScope,
            _ => Type::Pool,
        };
        if ex_ty != expected_ty {
            let art = if name == "task_spawn" { "an" } else { "a" };
            return Err(self.fail(&format!("{name} expects {art} {} handle as its first argument", expected_ty.name(self.ctx))));
        }
        self.take(Kind::Comma)?;
        let (callable, callable_ty) = self.spawn_callable_with(arguments.get(1), slots)?;
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        let mut capture_slots = Vec::new();
        let cid = match (&callable, callable_ty) {
            (Expr::ClosureLit(cid), Type::Closure(_)) | (Expr::Variable(_, Type::Closure(cid), ..), _) => {
                let def = self.generics.closures[*cid].clone();
                if !def.params.is_empty() {
                    return Err(self.fail("the spawned task closure must take no parameters"));
                }
                if def.ret != Type::Int {
                    return Err(self.fail("the spawned task closure must return i64"));
                }
                if !def.borrow_captures.is_empty() {
                    return Err(self.fail("task_spawn moves its captures into the task; borrow captures are not allowed"));
                }
                for (outer_id, ty) in &def.value_captures {
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF | Type::Struct(_) | Type::Enum(..) | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool) {
                        return Err(self.fail("task_spawn moves its captures into the task; this capture's storage cannot cross"));
                    }
                    if self.moved.contains(outer_id) {
                        return Err(moved_value_hint(self.error_at(at, "cannot spawn a task that captures a moved variable").with_code("E0102")));
                    }
                    if self.is_borrowed(*outer_id) {
                        return Err(self.fail("cannot spawn a task while one of the closure's captured variables is borrowed"));
                    }
                    if ty.owned() { self.moved.insert(*outer_id); }
                    capture_slots.push(Expr::Variable(*outer_id, *ty, 0, 0));
                }
                self.ctx.record_spawned(crate::frontend::closures::SpawnedThread::Closure {
                    fn_id: self.ctx.closure_fn_id(*cid),
                    captures: def.value_captures.iter().map(|(_, ty)| *ty).collect(),
                    borrows: Vec::new(),
                    ret: def.ret,
                });
                *cid
            }
            _ => return Err(self.fail("task_spawn expects a zero-parameter closure returning i64")),
        };
        Ok(match name {
            "task_spawn" => Statement::TaskSpawn(Box::new(ex), capture_slots, Box::new(callable), cid, line, col),
            "scope_spawn" => Statement::ScopeSpawn(Box::new(ex), capture_slots, Box::new(callable), cid, line, col),
            _ => Statement::PoolSpawn(Box::new(ex), capture_slots, Box::new(callable), cid, line, col),
        })
    }

    /// The type a method-call chain computes (`start.shifted(1, 2).shifted(0, -6)`).
    /// The precompute walks the chain left to right: the root is a parameter,
    /// `self`, or a binding the scan already typed, and each link asks the
    /// inherent method's declared return type. The pointer surface is answered
    /// the same way (`let v = raw as *mut f64`, `let p = buf.offset(1)`), and a
    /// direct call answers by the callee's declared return (`extern "C" fn
    /// ae_std_alloc(..) -> *mut i64`). Only shapes the resolved route admits
    /// answer (arity included); anything else — a generic instance or a
    /// receiver the scan cannot name — stays typed.
    pub(super) fn chain_call_type(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        local_structs: &HashMap<String, Type>,
    ) -> Option<Type> {
        use ae_syntax::expression::ExpressionKind;
        match &expression.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => self
                .names
                .get(name)
                .map(|&(_, _, ty)| ty)
                .or_else(|| local_structs.get(name).copied()),
            // A literal initializer types its binding (the precompute records
            // it, so an address cast can check its pointee against it).
            ExpressionKind::Atom(Kind::Number(_)) => Some(Type::Int),
            ExpressionKind::Atom(Kind::Float(_)) => Some(Type::F64),
            ExpressionKind::Atom(Kind::True) | ExpressionKind::Atom(Kind::False) => Some(Type::Bool),
            ExpressionKind::Group(inner) => self.chain_call_type(inner, local_structs),
            ExpressionKind::Field { base, name } => {
                // A field read types its chain: the base's struct supplies the
                // field's declared type (`self.ptr` is *mut i64, `buf.len` i64).
                let ty = self.chain_call_type(base, local_structs)?;
                let sid = match ty {
                    Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                    _ => return None,
                };
                field_ty_in(sid, name, self.structs)
            }
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                // The address stays; the pointee spelling names the new view.
                if !matches!(self.chain_call_type(base, local_structs)?, Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                let name = self.source.get(pointee.clone())?;
                let pointee = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))?;
                let id = self.ctx.intern_pointee(pointee);
                Some(if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) })
            }
            ExpressionKind::AddressCast { binding, mutable, pointee, .. } => {
                // `&value as *const i64`: the operand's scalar type is the
                // pointee spelling, and the binding becomes a pointer local.
                let ty = self.names.get(binding).map(|&(_, _, ty)| ty)
                    .or_else(|| local_structs.get(binding).copied())?;
                let name = self.source.get(pointee.clone())?;
                let pointee = super::types::scalar_type(name)?;
                if pointee != ty {
                    return None;
                }
                let id = self.ctx.intern_pointee(pointee);
                Some(if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) })
            }
            ExpressionKind::Call { callee, arguments } => match &callee.kind {
                ExpressionKind::Field { base, name } => {
                    let ty = self.chain_call_type(base, local_structs)?;
                    if let Type::Ptr(id) | Type::PtrMut(id) = ty {
                        // A pointer method: `offset`/`byte_offset` keep the
                        // pointer type, `deref` answers with the pointee.
                        return match (name.as_str(), arguments.len()) {
                            ("offset" | "byte_offset", 1) => Some(ty),
                            ("deref", 0) => Some(self.ctx.ptr_pointee(id)),
                            _ => None,
                        };
                    }
                    let (signature, _, _) = self.resolve_inherent_method(name, ty)?;
                    (arguments.len() + 1 == signature.params.len()).then_some(signature.ret)
                }
                ExpressionKind::Atom(Kind::Ident(name)) if !RESERVED.contains(&name.as_str()) => {
                    let signature = self.functions.get(name)?;
                    (arguments.len() == signature.params.len()).then_some(signature.ret)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// A method call on a bare struct variable (`self.m(...)` included).
    /// `signature.params[0]` is the receiver; the argument list follows it.
    /// The receiver bookkeeping mirrors the typed postfix path, so the
    /// borrow graph and the pinned diagnostics stay identical.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lower_method_call(
        &mut self,
        method_name: &str,
        slot: usize,
        base_ty: Type,
        signature: Signature,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let wants_mut = matches!(signature.params.first(), Some(Type::StructMut(_)));
        let wants_ref = matches!(signature.params.first(), Some(Type::StructRef(_)));
        self.seek_scalar_offset(expression.range.end);
        if (wants_mut || wants_ref) && self.moved.contains(&slot) {
            return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow the receiver of '{method_name}' because it was moved")).with_code("E0102"), slot)));
        }
        if wants_mut {
            // A read-only receiver cannot grant a mutable borrow; the pinned
            // diagnostic reads exactly like the typed route's.
            if matches!(base_ty, Type::StructRef(_)) {
                return Err(self.fail(&format!("'{method_name}' borrows its receiver mutably, which a read-only borrow cannot grant; call it on the owned variable")));
            }
            let mutable = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(_, (_, mutable, _))| *mutable).unwrap_or(false);
            if !mutable {
                return Err(self.fail(&format!("'{method_name}' borrows its receiver mutably, but the variable was declared without 'var'")));
            }
            if self.is_borrowed(slot) {
                if !self.only_view_borrows_active(slot) {
                    return Err(self.fail(&format!("cannot borrow the receiver of '{method_name}' mutably while it is borrowed in this statement")));
                }
                let receiver_name = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(name, _)| name.clone()).unwrap_or_else(|| format!("v{slot}"));
                // The typed route records the conflict at the last token
                // before the statement's end (the call's closing paren).
                let conflict_at = self.tokens.partition_point(|token| token.offset < expression.range.end).saturating_sub(1);
                let conflict_at = self.tokens.get(conflict_at).map(|token| token.offset).unwrap_or(expression.range.start);
                self.deferred_write_conflicts.push((slot, conflict_at, receiver_name, "mutate", None));
            }
            self.mut_borrows.push((slot, self.scopes.len()));
        } else if wants_ref {
            if self.mut_borrows.iter().any(|&(bid, _)| bid == slot) {
                return Err(self.fail(&format!("cannot borrow the receiver of '{method_name}' while it is mutably borrowed in this statement")));
            }
            if !self.is_borrowed(slot) {
                self.call_borrows.push((slot, self.scopes.len()));
            }
        } else {
            // A by-value receiver moves: the typed postfix path's rules.
            if self.is_borrowed(slot) && !self.all_borrows_are_views(slot) {
                return Err(self.fail(&format!("cannot move the receiver of '{method_name}' while it is borrowed")));
            }
            if self.is_borrowed(slot) {
                let receiver_name = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(name, _)| name.clone()).unwrap_or_else(|| format!("v{slot}"));
                let conflict_at = self.tokens.partition_point(|token| token.offset < expression.range.end).saturating_sub(1);
                let conflict_at = self.tokens.get(conflict_at).map(|token| token.offset).unwrap_or(expression.range.start);
                self.deferred_write_conflicts.push((slot, conflict_at, receiver_name, "move", None));
            }
            if self.moved.contains(&slot) {
                return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot use a moved value as the receiver of '{method_name}'")).with_code("E0102"), slot)));
            }
            self.moved.insert(slot);
        }
        let mut args = if wants_mut || wants_ref {
            vec![Argument::Borrow(slot, matches!(base_ty, Type::Struct(_)))]
        } else {
            vec![Argument::Value(Expr::Variable(slot, base_ty, 0, 0))]
        };
        for (index, argument) in arguments.iter().enumerate() {
            self.seek_scalar_offset(argument.range.start);
            let Some(expected) = signature.params.get(index + 1).copied() else {
                return Err(self.fail(&format!("too many arguments to '{method_name}'")));
            };
            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
            self.check_value_argument_type(method_name, index, expected, actual, Some(&argument.range))?;
            if actual.owned()
                && let Expr::Variable(source, ..) = &value {
                    self.moved.insert(*source);
                }
            args.push(Argument::Value(value));
        }
        self.seek_scalar_offset(expression.range.end);
        Ok((Expr::Call(signature.id, args, signature.ret), signature.ret))
    }

    pub(super) fn lower_scalar_array_index(
        &mut self,
        index: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(index, slots)?;
        let scalar = self.checked_scalar(Some(&index.range)).or_else(|| ty.scalar());
        self.check_array_index(scalar, Some(index.range.end))?;
        Ok(value)
    }

    pub(super) fn lower_propagating_call(
        &mut self,
        operand: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        // A direct call or a variant construction the shared route already
        // lowers needs no typed token parse; anything else keeps the fallback.
        let shared = self.shared_propagation_operand(operand);
        let (value, ty) = match self.lower_shared_value(Some(Kind::Question), true) {
            Some(value) => value,
            None if shared => {
                self.seek_scalar_offset(operand.range.start);
                let lowered = self.lower_scalar_body_expression(operand, slots)?;
                self.seek_scalar_offset(operand.range.end);
                lowered
            }
            None => self.unsupported_expression()?,
        };
        if *self.current() != Kind::Question {
            return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
        }
        let (statement, _) = self.propagate_statement(value, ty, None)?;
        self.pos += 1;
        self.take(Kind::Semi)?;
        Ok(statement)
    }

    /// The spawn callable, with the shared callable node when the resolved
    /// route has it. A parenthesized or bare closure variable lowers from the
    /// tree; a closure literal keeps the typed closure machinery (capture
    /// inference); anything else keeps the typed token parse.
    pub(super) fn spawn_callable_with(
        &mut self,
        callable: Option<&ae_syntax::expression::Expression>,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        if let Some(argument) = callable {
            let inner = match &argument.kind {
                ExpressionKind::Group(inner) => inner.as_ref(),
                _ => argument,
            };
            // A closure or function value bound to a name, or any atom-shaped
            // callable (a literal, `true`, `0`): the shared expression lowering
            // answers it, and the caller's callable rules (closure capture,
            // function value, or the typed route's "needs a closure value")
            // take it from there.
            let atom = matches!(&inner.kind, ExpressionKind::Atom(_));
            let named_callable = match &inner.kind {
                ExpressionKind::Atom(Kind::Ident(name)) => matches!(self.names.get(name), Some(&(_, _, Type::Closure(_) | Type::Fn(_)))),
                _ => false,
            };
            if atom || named_callable {
                self.seek_scalar_offset(argument.range.start);
                let lowered = self.lower_scalar_body_expression(argument, slots)?;
                self.seek_scalar_offset(argument.range.end);
                return Ok(lowered);
            }
        }
        if let Some(header) = self.shared_closure_header() {
            let end = self.tokens.partition_point(|token| token.offset < header.range.end);
            if self.tokens.get(end).is_some_and(|token| token.kind == Kind::RParen)
                && self.expression_depth < 128
            {
                if self.expression_depth == 0 {
                    self.expression_nodes = 0;
                }
                self.expression_depth += 1;
                self.expression_nodes += 1;
                let result = if self.expression_nodes > 128 {
                    Err(self.fail("expression complexity limit exceeded (128)"))
                } else {
                    self.closure_literal()
                };
                self.expression_depth -= 1;
                return result;
            }
        }
        self.seek_scalar_offset(self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0));
        Err(self.fail("a spawn callable must be a closure literal or a closure/function-value name").with_code("E0011"))
    }

}
