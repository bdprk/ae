//! Closure literals: capture inference and the generated call shape (ADR-0188).
//!
//! A closure literal compiles to an ordinary function whose parameter list is
//! the captured variables (hidden, leading) followed by the visible ones. The
//! classification is Rust's Fn/FnOnce split, inferred from one throwaway body
//! parse: a capture the body *moves* is a value capture — every call moves the
//! outer variable into the callee — and a capture the body only *reads* is a
//! borrow capture, forwarded as the read-only borrow form of its type. A
//! closure value itself has no storage, so it cannot be stored, returned, or
//! assigned; the enclosing scope keeps every variable the body borrowed.
use super::*;
use super::parser::moved_value_hint;

// One spawn surface: a closure's generated function with its capture types in
// order, or a zero-parameter function value keyed by its interned signature.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum SpawnedThread {
    // The context's leading fields, in order: the value captures (moved) and
    // the borrow captures (read-only views/refs the join keeps alive).
    Closure { fn_id: usize, captures: Vec<Type>, borrows: Vec<(usize, Type)>, ret: Type },
    FnValue { sig: usize, ret: Type },
}

impl Parser<'_> {
    // `|a: i64, b: i64| -> R { body }` — pos sits on the opening pipe.
    pub(super) fn closure_literal(&mut self) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let lit_offset = self.tokens[self.pos].offset;
        // Pass 1 binds every name of the enclosing scope so the body parses
        // once; the walk afterwards says which of them the body actually
        // used, and the moved set says which of those it consumed.
        let mut outer: Vec<(String, usize, bool, Type)> = self.names.iter()
            .map(|(name, &(id, mutable, ty))| (name.clone(), id, mutable, ty))
            .collect();
        // Keep generated capture slots in binding order across compiler runs.
        outer.sort_unstable_by_key(|(_, id, _, _)| *id);
        // Both forms start their body range ON the opening token: the pipe
        // form's parameter section consumes the pipe, the Or form's skips
        // Or and the arrow beside it.
        let body_start = self.pos;
        // Two probes decide the capture modes. The first binds every
        // borrowable capture in its read-only form, so a body that only reads
        // gets borrow captures and the closure stays callable any number of
        // times; if that body fails — because some capture is used where an
        // owned value is required, like a returned string — the second probe
        // binds everything owned and every capture becomes a value capture
        // that its call moves (FnOnce, which is sound for the same reason a
        // plain call is).
        let borrow_probe: Vec<(String, Type)> = outer.iter()
            .map(|(name, _, _, ty)| (name.clone(), borrow_capture_form(ty).unwrap_or(*ty)))
            .collect();
        let id1 = self.generics.next_fn_id;
        self.generics.next_fn_id += 1;
        let probe = self.parse_closure_body(id1, body_start, &borrow_probe);
        let (probe, value_mode) = match probe {
            Ok((function, _moved, _)) => (function, false),
            Err(_) => {
                let id1b = self.generics.next_fn_id;
                self.generics.next_fn_id += 1;
                let owned_probe: Vec<(String, Type)> = outer.iter()
                    .map(|(name, _, _, ty)| (name.clone(), *ty))
                    .collect();
                let (function, _moved, _) = self.parse_closure_body(id1b, body_start, &owned_probe)?;
                (function, true)
            }
        };
        let (used, mutated) = collect_capture_uses(&probe);
        // The probe bound its captures in order as its leading parameters, so
        // a used parameter slot names exactly one candidate capture.
        let capture_names: Vec<&String> = probe.params.iter().take(borrow_probe.len())
            .zip(borrow_probe.iter())
            .filter(|((var_id, _), _)| used.contains(var_id))
            .map(|(_, (name, _))| name)
            .collect();
        let mutated_names: Vec<&String> = probe.params.iter().take(borrow_probe.len())
            .zip(borrow_probe.iter())
            .filter(|((var_id, _), _)| mutated.contains(var_id))
            .map(|(_, (name, _))| name)
            .collect();
        let mut value_captures: Vec<(usize, Type)> = Vec::new();
        let mut borrow_captures: Vec<(usize, Type)> = Vec::new();
        let mut mut_captures: Vec<(usize, Type)> = Vec::new();
        let mut bindings: Vec<(String, Type)> = Vec::new();
        for (name, outer_id, mutable, ty) in &outer {
            if !capture_names.contains(&name) { continue; }
            if mutated_names.contains(&name) {
                if !*mutable || !is_copy_type(*ty, self.structs, self.enums) {
                    return Err(self.error_at(lit_offset, &format!(
                        "a closure cannot mutate captured variable '{name}' in this version: captures are by value or read-only, so write the loop or the update in the enclosing scope")));
                }
                if self.moved.contains(outer_id) {
                    return Err(moved_value_hint(self.binding_label(self.error_at(lit_offset, &format!("cannot capture '{name}' because it was moved")).with_code("E0102"), *outer_id)));
                }
                mut_captures.push((*outer_id, *ty));
                continue;
            }
            if self.moved.contains(outer_id) {
                return Err(moved_value_hint(self.binding_label(self.error_at(lit_offset, &format!("cannot capture '{name}' because it was moved")).with_code("E0102"), *outer_id)));
            }
            match (value_mode, borrow_capture_form(ty)) {
                (false, Some(hidden_ty)) => borrow_captures.push((*outer_id, hidden_ty)),
                _ => value_captures.push((*outer_id, *ty)),
            }
        }
        // The generated function's parameter list is the capture list in
        // call order — values first, then borrows — and the call site builds
        // its arguments in exactly that order.
        let names_by_id: std::collections::HashMap<usize, String> = outer.iter()
            .map(|(name, id, _, _)| (*id, name.clone()))
            .collect();
        for (outer_id, ty) in &value_captures {
            bindings.push((names_by_id[outer_id].clone(), *ty));
        }
        for (outer_id, ty) in &borrow_captures {
            bindings.push((names_by_id[outer_id].clone(), *ty));
        }
        for (outer_id, ty) in &mut_captures {
            bindings.push((names_by_id[outer_id].clone(), *ty));
        }
        // Pass 2 parses the same tokens with the classified capture types, so
        // the emitted body reads borrows as borrows and values as owned slots.
        let id2 = self.generics.next_fn_id;
        self.generics.next_fn_id += 1;
        let (function, _, end_pos) = self.parse_closure_body(id2, body_start, &bindings)?;
        // The enclosing parser continues after the literal's closing brace.
        self.pos = end_pos;
        // Only the visible parameters are call-checkable; the leading capture
        // slots are the call site's business, not the caller's argument list.
        let capture_count = value_captures.len() + mut_captures.len() + borrow_captures.len();
        let visible: Vec<Type> = function.params[capture_count..].iter().map(|&(_, ty)| ty).collect();
        let ret = function.returns;
        self.pending.push(function);
        // The enclosing scope lends every borrow capture for as long as the
        // closure name is live: the outer variable may not move or be written
        // behind the closure's back (the same rule a string view gets).
        for (outer_id, _) in &borrow_captures {
            if !self.is_borrowed(*outer_id) {
                self.borrows.push((*outer_id, self.scopes.len(), BorrowKind::Container));
            }
        }
        let kind = if !mut_captures.is_empty() {
            CallableKind::FnMut
        } else if value_captures.iter().any(|(_, ty)| !is_copy_type(*ty, self.structs, self.enums)) {
            CallableKind::FnOnce
        } else {
            CallableKind::Fn
        };
        let cid = self.generics.closures.len();
        self.ctx.push_closure(id2, borrow_captures.clone());
        self.generics.closures.push(ClosureDef {
            fn_id: id2,
            params: visible,
            ret,
            value_captures,
            borrow_captures,
            mut_captures,
            kind,
            env: None,
            file: self.file.clone(),
        });
        Ok((Expr::ClosureLit(cid), Type::Closure(cid)))
    }

    // Each pass binds its candidates as hidden parameters in capture order.
    // Use the outer declaration offsets to recover their resolved identities.
    fn parse_closure_body(&mut self, id: usize, body_start: usize, captures: &[(String, Type)]) -> Result<(Function, std::collections::HashSet<usize>, usize), ae_span::Diagnostic> {
        let capture_starts: Vec<_> = captures.iter().map(|(name, _)| {
            self.names.get(name).and_then(|&(slot, _, _)| {
                self.binding_ranges.get(&slot).map(|range| range.start)
                    // A nested closure capturing a capture: the middle
                    // closure's hidden parameter has no name range, so its
                    // origin is the binding it was captured from.
                    .or_else(|| self.capture_origins.get(&slot).copied())
            })
        }).collect();
        let mut sub = Parser {
            file: self.file.clone(), source: self.source, tokens: Rc::clone(&self.tokens), shared_source: Rc::clone(&self.shared_source), pos: body_start,
            names: HashMap::new(), binding_ranges: HashMap::new(), capture_origins: HashMap::new(), resolved_expressions: Rc::clone(&self.resolved_expressions), next_var: 1, expression_depth: 0, expression_nodes: 0, block_depth: 0,
            loop_depth: 0, loop_scopes: Vec::new(), loop_labels: Vec::new(), next_label: 0, unsafe_depth: 0,
            functions: &mut *self.functions, structs: &mut *self.structs, struct_names: &mut *self.struct_names,
            enums: &mut *self.enums, enum_names: &mut *self.enum_names, generics: &mut *self.generics, ctx: &mut *self.ctx,
            id_order: Vec::new(), in_main: false, return_type: Type::Int(IntTy::I64), moved: std::collections::HashSet::new(),
            borrows: Vec::new(), deferred_write_conflicts: Vec::new(), deferred_borrow_spawns: Vec::new(), call_borrows: Vec::new(), mut_borrows: Vec::new(), scopes: Vec::new(), owned_params: Vec::new(),
            subst: HashMap::new(), trait_bounds: HashMap::new(), receiver: None, impl_self: None, inherent_impl: false,
            // Shadowing stays allowed in both passes: a body-local may reuse
            // an outer name, and the AST walk (not the name map) decides what
            // was captured.
            shadow_ok: true, impl_fn_id: None, fn_once_type_params: std::collections::HashSet::new(), fn_once_slots: std::collections::HashSet::new(), pending: Vec::new(),
        };
        let parsed = sub.closure_body(id, captures, &capture_starts);
        let end_pos = sub.pos;
        let sub_moved = std::mem::take(&mut sub.moved);
        let mut nested = std::mem::take(&mut sub.pending);
        drop(sub);
        let function = parsed?;
        let moved = sub_moved;
        self.pending.append(&mut nested);
        Ok((function, moved, end_pos))
    }

    // `(a: i64, b: i64) -> R { body }` for one closure, starting after the
    // opening pipe. Parameter types are required — the language has no
    // inference for parameter annotations, and the return type is what makes
    // the call site checkable.
    // Called on a fresh sub-parser whose names already hold the pre-bound
    // captures (and, in the probe pass, the whole enclosing scope); nothing
    // here may clear that state. The captures are the hidden leading
    // parameters in exactly the order the call site will pass them, so they
    // are added to the signature before the visible ones are parsed.
    fn closure_body(&mut self, id: usize, captures: &[(String, Type)], capture_starts: &[Option<usize>]) -> Result<Function, ae_span::Diagnostic> {
        let mut params: Vec<(usize, Type)> = Vec::new();
        let mut resolved_captures = Vec::new();
        for ((name, ty), start) in captures.iter().zip(capture_starts.iter().copied()) {
            let var_id = self.next_var; self.next_var += 1;
            // Capture slots parse as mutable so the probe reaches every
            // mutation site and the walker can reject it by name; the
            // classification runs before the emit pass, so the emitted body
            // can never actually write one.
            self.names.insert(name.clone(), (var_id, true, *ty));
            params.push((var_id, *ty));
            if let Some(start) = start {
                // Remember where this hidden parameter was captured from, so
                // a closure nested inside this body can resolve the same
                // binding (its own capture_starts lookup walks this map).
                self.capture_origins.insert(var_id, start);
                resolved_captures.push((var_id, start));
            }
        }
        let shared = self.shared_closure_header();
        let header = shared.as_deref().and_then(|expression| match &expression.kind {
            ae_syntax::expression::ExpressionKind::Closure { params, return_type, .. } => {
                Some((params.as_slice(), return_type.start))
            }
            _ => None,
        });
        if *self.current() == Kind::Or {
            // `||`: the empty parameter list. The arrow sits beside it.
            self.pos += 2;
        } else if *self.current() == Kind::Pipe {
            self.pos += 1;
            loop {
                if params.len() >= 16 { return Err(self.fail("closure parameter limit exceeded (16)")); }
                let shared_param = header.and_then(|(visible, _)| visible.get(params.len() - captures.len()))
                    .filter(|param| self.tokens[self.pos].offset == param.name_range.start);
                let (param, binding_range) = if let Some(param) = shared_param {
                    self.pos += 1;
                    (param.name.clone(), param.name_range.clone())
                } else {
                    let binding_range = self.tokens[self.pos].offset..self.tokens[self.pos].end;
                    (self.identifier()?, binding_range)
                };
                if self.names.contains_key(&param) {
                    if self.shadow_ok {
                        self.names.remove(&param);
                    } else {
                        return Err(self.duplicate_binding_error(&param));
                    }
                }
                if RESERVED.contains(&param.as_str()) { return Err(self.fail(&format!("'{param}' is a built-in name and cannot be a closure parameter"))); }
                if shared_param.is_some_and(|param| self.tokens[self.pos + 1].offset == param.ty.start) {
                    self.pos += 1;
                } else {
                    self.take(Kind::Colon)?;
                }
                let ty = self.callable_parameter_type()?;
                let var_id = self.next_var; self.next_var += 1;
                self.names.insert(param, (var_id, matches!(ty, Type::StructMut(_)), ty));
                self.binding_ranges.insert(var_id, binding_range);
                params.push((var_id, ty));
                if *self.current() != Kind::Comma { break; }
                self.pos += 1;
            }
            if header.is_some_and(|(_, return_start)| {
                *self.current() == Kind::Pipe
                    && self.tokens.get(self.pos + 2).is_some_and(|token| token.offset == return_start)
            }) {
                self.pos += 2;
            } else {
                self.take(Kind::Pipe)?;
                self.take(Kind::Arrow)?;
            }
        }
        let ret = self.return_annotation()?;
        // A closure may not return a borrow: the hidden capture parameters
        // live in the enclosing frame, and the borrow would outlive them the
        // moment the call returns.
        if matches!(ret, Type::StructRef(_) | Type::StructMut(_)) {
            return Err(self.fail("a closure cannot return a borrow; return an owned value instead"));
        }
        self.return_type = ret;
        self.in_main = false;
        self.scopes.push(params.iter().map(|&(var_id, ty)| (var_id, ty)).collect());
        let body = match self.lower_resolved_body_with_captures(&resolved_captures) {
            Some(body) => body,
            None => {
                // The resolved route is the body parser (P1.4): a closure body
                // it does not take is a diagnostic, never a typed fallback —
                // the capture classification can only answer for a body that
                // lowers.
                if std::env::var_os("AE_ROUTE_REPORT").is_some() {
                    let offset = self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0);
                    eprintln!("route legacy closure id={id} at {offset}");
                }
                let offset = self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0);
                return Err(self.error_at(offset, "this closure's body is not on the resolved route yet; the legacy body parser has been removed"));
            }
        };
        let scope = self.scopes.pop().unwrap();
        let mut body = body?;
        let owned: Vec<(usize, Type)> = scope.iter().rev().filter(|(_, ty)| self.is_owned(*ty)).copied().collect();
        if !owned.is_empty() { body.push(Statement::Frees(owned)); }
        if !returns(&body) { return Err(self.fail("a closure body must return a value on every path")); }
        // The closure's deferred conflicts belong to the closure's own graph;
        // the enclosing function keeps the ones it deferred before.
        let saved = std::mem::take(&mut self.deferred_write_conflicts);
        let function = Function { id, params, body, is_main: false, returns: ret };
        let adjudication = self.adjudicate_deferred_conflicts(&function);
        self.deferred_write_conflicts.extend(saved);
        adjudication?;
        Ok(function)
    }


    /// A closure call whose visible arguments come from the shared call node:
    /// the captured slots are prepended exactly as the typed path does, then
    /// the same value/borrow/move rules a direct call uses lower the visible
    /// list. A shape the rules cannot cover is a diagnostic.
    pub(super) fn closure_call_with(
        &mut self,
        name: &str,
        call_offset: usize,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let Some(&(_, _, Type::Closure(cid))) = self.names.get(name) else {
            return Err(self.fail(&format!("internal: '{name}' is not a closure")));
        };
        let def = self.generics.closures[cid].clone();
        let mut args: Vec<Argument> = Vec::new();
        for (outer_id, ty) in &def.value_captures {
            if self.moved.contains(outer_id) {
                return Err(moved_value_hint(self.binding_label(self.error_at(call_offset, &format!("cannot call '{name}' again because a value capture was already moved by the first call")).with_code("E0102"), *outer_id)));
            }
            if self.is_borrowed(*outer_id) {
                return Err(self.fail(&format!("cannot call '{name}' while one of its captured variables is borrowed")));
            }
            if self.is_owned(*ty) {
                self.moved.insert(*outer_id);
            }
            args.push(Argument::Value(Expr::Variable(*outer_id, *ty, 0, 0)));
        }
        for (outer_id, hidden) in &def.borrow_captures {
            match hidden {
                Type::StrView => args.push(Argument::View(Expr::Variable(*outer_id, Type::String, 0, 0))),
                _ => args.push(Argument::Borrow(*outer_id, true)),
            }
        }
        let visible = self.lower_value_arguments(&format!("closure {name}"), arguments, &def.params, slots)?;
        args.extend(visible);
        Ok((Expr::Call(def.fn_id, args, def.ret), def.ret))
    }
}

// The read-only form of a capture: the borrow types a body can read through.
// Scalars have no borrow form, and maps cannot be borrowed, so they always
// travel by value.
fn borrow_capture_form(ty: &Type) -> Option<Type> {
    match ty {
        Type::String => Some(Type::StrView),
        Type::Vector(id) => Some(Type::VectorRef(*id)),
        Type::Struct(sid) => Some(Type::StructRef(*sid)),
        _ => None,
    }
}

// Which variables a parsed body actually touches, and which of them it
// mutates. A capture is a variable the body reads; a mutation through a
// capture would silently write a copy, so it is rejected instead.
fn collect_capture_uses(function: &Function) -> (std::collections::HashSet<usize>, std::collections::HashSet<usize>) {
    let mut used = std::collections::HashSet::new();
    let mut mutated = std::collections::HashSet::new();
    walk_statements(&function.body, &mut used, &mut mutated);
    (used, mutated)
}

// Depth-first over the statement list. Every expression walks; every slot a
// statement writes — assignments, element stores, container updates — is a
// mutation of that slot, which is what the capture check rejects.
fn walk_statements(statements: &[Statement], used: &mut std::collections::HashSet<usize>, mutated: &mut std::collections::HashSet<usize>) {
    for statement in statements {
        match statement {
            Statement::Unsafe(inner) => walk_statements(inner, used, mutated),
            // A pointer write uses the pointer variable and the value; the
            // target storage is untracked (M2.12).
            Statement::PtrStore(id, value, ..) => { used.insert(*id); walk_expr(value, used, mutated); }
            // An atomic store reads the handle and the value; the cell is
            // shared by copy, so neither side is a mutation of tracked state.
            Statement::AtomicStore(handle, value) | Statement::SharedSet(handle, value, _) => { walk_expr(handle, used, mutated); walk_expr(value, used, mutated); }
            Statement::ChanClose(handle) => walk_expr(handle, used, mutated),
            Statement::TaskSpawn(ex, slots, callable, ..) | Statement::ScopeSpawn(ex, slots, callable, ..) | Statement::PoolSpawn(ex, slots, callable, ..) => {
                walk_expr(ex, used, mutated);
                for s in slots { walk_expr(s, used, mutated); }
                walk_expr(callable, used, mutated);
            }
            Statement::TaskYield => {}
            Statement::Return(expr, _) | Statement::Declare(_, expr) => walk_expr(expr, used, mutated),
            Statement::Array(_, values, _) => for v in values { walk_expr(v, used, mutated); },
            Statement::Assign(id, expr, _) => { mutated.insert(*id); walk_expr(expr, used, mutated); }
            Statement::Store(id, _, index, value, _, _, _) => { mutated.insert(*id); walk_expr(index, used, mutated); walk_expr(value, used, mutated); }
            Statement::StrVecPush(id, value, _, _, _) | Statement::GVecPush(id, value, _, _, _) => { mutated.insert(*id); walk_expr(value, used, mutated); }
            Statement::VecPushField(id, _, _, value, ..) => { mutated.insert(*id); walk_expr(value, used, mutated); }
            Statement::VecStoreField(id, _, _, index, value, ..) => { mutated.insert(*id); walk_expr(index, used, mutated); walk_expr(value, used, mutated); }
            Statement::VecPopField(id, ..) | Statement::VecSortField(id, ..) => { mutated.insert(*id); }
            Statement::Sort(id, ..) | Statement::Reverse(id, ..) | Statement::StrVecPop(id, ..) | Statement::GVecPop(id, ..) => { mutated.insert(*id); }
            Statement::StrVecStore(id, index, value, _, _, _) | Statement::GVecStore(id, index, value, _, _, _) | Statement::SliceStore(id, index, value, _, _, _) => { mutated.insert(*id); walk_expr(index, used, mutated); walk_expr(value, used, mutated); }
            Statement::FieldStore(id, _, _, value, _) => { mutated.insert(*id); walk_expr(value, used, mutated); }
            Statement::Print(printed) => if let Printed::Expr(expr) = printed { walk_expr(expr, used, mutated); },
            Statement::Assert(expr, _, _) => walk_expr(expr, used, mutated),
            Statement::If(cond, yes, no) => { walk_expr(cond, used, mutated); walk_statements(yes, used, mutated); walk_statements(no, used, mutated); }
            Statement::While(cond, body) => { walk_expr(cond, used, mutated); walk_statements(body, used, mutated); }
            Statement::For(_, container, body, ..) => { used.insert(*container); walk_statements(body, used, mutated); }
            Statement::ForTokens(cursor, _, body, ..) => { used.insert(*cursor); walk_statements(body, used, mutated); }
            Statement::Match(subject, _, arms) => {
                used.insert(*subject);
                for arm in arms { walk_statements(&arm.body, used, mutated); }
            }
            // CFG-only (M2.3): a payload binding reads its subject.
            Statement::BindPayload(_, subject, ..) | Statement::TakePropagationPayload(subject, ..) => { used.insert(*subject); }
            Statement::MapPut(place, key, value, _, _) => { record_place(place, used, mutated); walk_expr(key, used, mutated); walk_expr(value, used, mutated); }
            Statement::ResetTokens(cursor, _) | Statement::AdvanceTokens(cursor, ..) => { used.insert(*cursor); }
            Statement::Propagate(expr, _, _, _, _) => walk_expr(expr, used, mutated),
            Statement::Exit(code, _) => walk_expr(code, used, mutated),
            Statement::Break | Statement::Continue(_) | Statement::Frees(_) => {}
        }
    }
}

fn walk_expr(expr: &Expr, used: &mut std::collections::HashSet<usize>, mutated: &mut std::collections::HashSet<usize>) {
    match expr {
        Expr::Variable(id, ..) => { used.insert(*id); }
        // A pointer takes the address of (or reads through) the slot (M2.12).
        Expr::BorrowAs(e, ..) | Expr::DerefPtr(e, ..) => walk_expr(e, used, mutated),
        // A field receiver and its index reference captured values.
        Expr::VecIndexOf(base, index, ..) => {
            walk_expr(base, used, mutated);
            walk_expr(index, used, mutated);
        }
        // A slice reads its owner; its index reads are ordinary reads.
        Expr::SliceOf(slot, ..) => { used.insert(*slot); }
        Expr::SliceIndex(slot, index, ..) => {
            used.insert(*slot);
            walk_expr(index, used, mutated);
        }
        // A sub-slice reads its owner and both bounds.
        Expr::SliceRange(slot, _, _, start, end, ..) => {
            used.insert(*slot);
            walk_expr(start, used, mutated);
            walk_expr(end, used, mutated);
        }
        // Both the pointer and its offset can reference captured values.
        Expr::PtrOffset(base, count, ..) | Expr::PtrWrite(base, count, ..) => {
            walk_expr(base, used, mutated);
            walk_expr(count, used, mutated);
        }
        // A retag uses the base without consuming it.
        Expr::PtrCast(e, ..) | Expr::PtrDropInPlace(e, ..) | Expr::ScalarCast(e, ..) => walk_expr(e, used, mutated),
        Expr::IntMethod(_, recv, arg, ..) => {
            walk_expr(recv, used, mutated);
            if let Some(arg) = arg { walk_expr(arg, used, mutated); }
        }
        Expr::Index(id, _, index, ..) | Expr::VecIndex(id, index, ..) | Expr::StrVecIndex(id, index, ..) => {
            used.insert(*id);
            walk_expr(index, used, mutated);
        }
        Expr::Tokens(id, _, sep, ..) => { used.insert(*id); walk_expr(sep, used, mutated); }
        Expr::Join(id, sep, ..) => { used.insert(*id); walk_expr(sep, used, mutated); }
        // The call's program name is an expression like any other; the
        // borrowed argument vector is a bare variable slot.
        Expr::RunCommand(prog, id, ..) => { walk_expr(prog, used, mutated); used.insert(*id); }
        Expr::Field(base, _, _) => walk_expr(base, used, mutated),
        Expr::FnCall(slot, args, ..) => {
            used.insert(*slot);
            for arg in args {
                match arg {
                    Argument::Value(e) | Argument::View(e) => walk_expr(e, used, mutated),
                    Argument::Borrow(id, _) => { used.insert(*id); }
                    Argument::FnPtr(..) => {}
                }
            }
        }
        Expr::FnExprCall(callee, args, ..) => {
            walk_expr(callee, used, mutated);
            for arg in args {
                match arg {
                    Argument::Value(e) | Argument::View(e) => walk_expr(e, used, mutated),
                    Argument::Borrow(id, _) => { used.insert(*id); }
                    Argument::FnPtr(..) => {}
                }
            }
        }
        Expr::Call(_, args, _) => {
            for arg in args {
                match arg {
                    Argument::Value(e) | Argument::View(e) => walk_expr(e, used, mutated),
                    Argument::Borrow(id, _) => { used.insert(*id); }
                    Argument::FnPtr(..) => {}
                }
            }
        }
        Expr::MapGet(place, key, ..) | Expr::MapHas(place, key, ..) => { used.insert(place.slot()); walk_expr(key, used, mutated); }
        Expr::MapRemove(place, key, ..) => { record_place(place, used, mutated); walk_expr(key, used, mutated); }
        Expr::MapInc(place, key, delta, ..) => { record_place(place, used, mutated); walk_expr(key, used, mutated); walk_expr(delta, used, mutated); }
        Expr::MapKeys(place, ..) => { used.insert(place.slot()); }
        Expr::Binary(a, _, b, ..) | Expr::StrEq(a, b, _) | Expr::Contains(a, b, ..) | Expr::Split(a, b, ..) | Expr::WriteFile(a, b, ..) | Expr::Slice(a, b, ..) | Expr::TryWriteFile(a, b, ..) | Expr::TryCopyFile(a, b, ..) | Expr::TryRenamePath(a, b, ..) | Expr::PathJoin(a, b, ..) | Expr::StartsWith(a, b) | Expr::EndsWith(a, b) | Expr::CountLinesWith(a, b, ..) | Expr::Utf8Prefix(a, b, ..) | Expr::Find(a, b, ..) | Expr::JsonGet(a, b, ..) | Expr::JsonInt(a, b, ..) | Expr::JsonBool(a, b, ..) | Expr::JsonLen(a, b, ..) | Expr::JsonHas(a, b, ..) => { walk_expr(a, used, mutated); walk_expr(b, used, mutated); }
        Expr::Neg(a, ..) | Expr::Not(a) | Expr::Len(a, ..) | Expr::ReadFile(a, ..) | Expr::Bytes(a, ..) | Expr::ReadFileBytes(a, _, _, ..) | Expr::Trim(a, ..) | Expr::Lines(a, ..) | Expr::Utf8Len(a, ..) | Expr::ToString(a, ..) | Expr::FloatToString(a, ..) | Expr::ToF64(a, ..) => walk_expr(a, used, mutated),
        Expr::SliceView(_, _, a, b, ..) => { walk_expr(a, used, mutated); walk_expr(b, used, mutated); }

        Expr::StructLit(_, values) | Expr::EnumLit(_, _, _, values) | Expr::VecLit(values, ..) => { for v in values { walk_expr(v, used, mutated); } }
        Expr::MathCall(_, args, ..) => { for v in args { walk_expr(v, used, mutated); } }
        Expr::TryReadFile(a, ..) | Expr::TryListDir(a, ..) | Expr::PathExists(a, ..) | Expr::PathIsDir(a, ..) | Expr::ParseInt(a, ..) | Expr::PathName(a, ..) | Expr::PathParent(a, ..) | Expr::TryFileInfo(a, ..) | Expr::TryRemoveFile(a, ..) | Expr::TryRemoveDir(a, ..) | Expr::TryCreateDir(a, ..) | Expr::SetCwd(a, ..) | Expr::Env(a, ..) | Expr::ToI64(a, ..) => walk_expr(a, used, mutated),
        Expr::TryAppendFile(a, b, ..) => { walk_expr(a, used, mutated); walk_expr(b, used, mutated); }
        // A spawn reads its callable and its capture slots (the slots move
        // into the thread — ADR-0214), so a nested spawn captures what its
        // slots and callable read.
        Expr::Spawn(slots, callable, ..) => {
            for slot in slots { walk_expr(slot, used, mutated); }
            walk_expr(callable, used, mutated);
        }
        Expr::Wait(handle, ..) => walk_expr(handle, used, mutated),
        // An atomic operation reads its handle (and its value operands);
        // copy semantics make every operand a read (spec/concurrency.md §8).
        Expr::AtomicNew(value) => walk_expr(value, used, mutated),
        Expr::AtomicLoad(handle) => walk_expr(handle, used, mutated),
        Expr::SharedNew(value, _) => walk_expr(value, used, mutated),
        Expr::SharedGet(handle, _) => walk_expr(handle, used, mutated),
        Expr::MutexNew => {}
        Expr::MutexLock(handle) => walk_expr(handle, used, mutated),
        Expr::ChanNew(cap) => walk_expr(cap, used, mutated),
        Expr::ChanSend(ch, val) | Expr::ChanTrySend(ch, val) | Expr::ChanSelect(ch, val) => { walk_expr(ch, used, mutated); walk_expr(val, used, mutated); }
        Expr::ChanRecv(ch) => walk_expr(ch, used, mutated),
        Expr::ExecNew | Expr::ScopeNew => {}
        Expr::ExecRun(ex) | Expr::PoolNew(ex) | Expr::PoolJoin(ex) => walk_expr(ex, used, mutated),
        Expr::AtomicAdd(handle, value) => { walk_expr(handle, used, mutated); walk_expr(value, used, mutated); }
        Expr::AtomicCas(handle, expect, new) => { walk_expr(handle, used, mutated); walk_expr(expect, used, mutated); walk_expr(new, used, mutated); }
        Expr::Number(_) | Expr::FloatLit(_) | Expr::BoolLit(_) | Expr::CharLit(_) | Expr::Unit | Expr::StrLit(..) | Expr::Args(..) | Expr::ReadStdin(..) | Expr::Cwd(..) | Expr::StrVecNew(..) | Expr::F64VecNew(..) | Expr::NowMs | Expr::NowS | Expr::ClosureLit(_) | Expr::FnRef(..) | Expr::MapNew(..) => {}
    }
}

fn record_place(place: &MapPlace, used: &mut std::collections::HashSet<usize>, mutated: &mut std::collections::HashSet<usize>) {
    let id = place.slot();
    used.insert(id);
    mutated.insert(id);
}

impl Parser<'_> {
    pub(super) fn materialize_closure_env(
        &mut self,
        cid: usize,
        at_offset: usize,
    ) -> Result<ClosureEnvInfo, ae_span::Diagnostic> {
        if let Some(info) = self.generics.closures[cid].env.clone() {
            return Ok(info);
        }
        let env_sid = self.structs.len();
        let wrapper_fn_id = self.generics.next_fn_id;
        self.generics.next_fn_id += 1;
        self.structs.push(StructDef {
            name: format!("__ae_closure_env_{cid}"),
            fields: Vec::new(),
            file: self.generics.closures[cid].file.clone(),
            public: false,
            repr_c: false,
            is_copy: false,
            drop_fn: None,
            drop_returns_unit: false,
        });
        let info = self.populate_closure_env(cid, env_sid, wrapper_fn_id, false, at_offset)?;
        self.generics.closures[cid].env = Some(info.clone());
        self.generics.closure_envs.insert(env_sid, info.clone());
        Ok(info)
    }

    pub(super) fn materialize_closure_struct_lit(
        &mut self,
        cid: usize,
        for_return: Option<(usize, usize)>,
        at_offset: usize,
    ) -> Result<(Expr, ClosureEnvInfo), ae_span::Diagnostic> {
        let info = if let Some((env_sid, wrapper_fn_id)) = for_return {
            self.populate_closure_env(cid, env_sid, wrapper_fn_id, true, at_offset)?
        } else {
            self.materialize_closure_env(cid, at_offset)?
        };
        let def = self.generics.closures[cid].clone();
        self.seek_scalar_offset(at_offset);
        let (line, column) = self.position();
        let mut field_exprs = Vec::new();
        for &(outer_id, ty) in &def.value_captures {
            self.check_variable_move(outer_id)?;
            if self.is_owned(ty) {
                self.moved.insert(outer_id);
            }
            field_exprs.push(Expr::Variable(outer_id, ty, line, column));
        }
        for &(outer_id, ty) in &def.mut_captures {
            self.check_variable_move(outer_id)?;
            if self.is_owned(ty) {
                self.moved.insert(outer_id);
            }
            field_exprs.push(Expr::Variable(outer_id, ty, line, column));
        }
        for &(outer_id, hidden_ty) in &def.borrow_captures {
            if for_return.is_some() {
                let outer_ty = self.names.values().find(|&&(id, _, _)| id == outer_id).map(|&(_, _, t)| t).unwrap_or(hidden_ty);
                self.check_variable_move(outer_id)?;
                if self.is_owned(outer_ty) {
                    self.moved.insert(outer_id);
                }
                field_exprs.push(Expr::Variable(outer_id, outer_ty, line, column));
            } else {
                let expr = match hidden_ty {
                    Type::StrView => Expr::SliceView(
                        outer_id,
                        None,
                        Box::new(Expr::Number(0)),
                        Box::new(Expr::Len(Box::new(Expr::Variable(outer_id, Type::String, line, column)), line, column)),
                        line,
                        column,
                    ),
                    _ => Expr::BorrowAs(Box::new(Expr::Variable(outer_id, hidden_ty, line, column)), false, line, column),
                };
                field_exprs.push(expr);
            }
        }
        Ok((Expr::StructLit(info.env_sid, field_exprs), info))
    }

    fn populate_closure_env(
        &mut self,
        cid: usize,
        env_sid: usize,
        wrapper_fn_id: usize,
        for_return: bool,
        at_offset: usize,
    ) -> Result<ClosureEnvInfo, ae_span::Diagnostic> {
        let def = self.generics.closures[cid].clone();
        let mut fields: Vec<(String, Type)> = Vec::new();
        let mut outer_tys: Vec<Type> = Vec::new();
        for (i, &(_, ty)) in def.value_captures.iter().enumerate() {
            if for_return && matches!(ty, Type::StrView | Type::VectorRef(_) | Type::StructRef(_) | Type::StructMut(_) | Type::ArrayOf(..)) {
                return Err(self.error_at(at_offset, "cannot return closure that borrows from the enclosing scope"));
            }
            fields.push((format!("c{i}"), ty));
            outer_tys.push(ty);
        }
        for (i, &(_, ty)) in def.mut_captures.iter().enumerate() {
            if for_return && matches!(ty, Type::StrView | Type::VectorRef(_) | Type::StructRef(_) | Type::StructMut(_) | Type::ArrayOf(..)) {
                return Err(self.error_at(at_offset, "cannot return closure that borrows from the enclosing scope"));
            }
            fields.push((format!("m{i}"), ty));
            outer_tys.push(ty);
        }
        for (i, &(outer_id, hidden_ty)) in def.borrow_captures.iter().enumerate() {
            let stored_ty = if for_return {
                let outer_ty = self.names.values()
                    .find(|&&(id, _, _)| id == outer_id)
                    .map(|&(_, _, t)| t)
                    .unwrap_or(hidden_ty);
                if matches!(outer_ty, Type::StrView | Type::VectorRef(_) | Type::StructRef(_) | Type::StructMut(_) | Type::ArrayOf(..)) {
                    return Err(self.error_at(at_offset, "cannot return closure that borrows from the enclosing scope"));
                }
                outer_ty
            } else {
                hidden_ty
            };
            fields.push((format!("b{i}"), stored_ty));
            outer_tys.push(stored_ty);
        }
        let is_copy = (for_return || def.borrow_captures.is_empty())
            && def.mut_captures.is_empty()
            && fields.iter().all(|(_, ty)| is_copy_type(*ty, self.structs, self.enums));
        self.structs[env_sid].fields = fields.clone();
        self.structs[env_sid].is_copy = is_copy;

        let env_param_ty = match def.kind {
            CallableKind::Fn => Type::StructRef(env_sid),
            CallableKind::FnMut => Type::StructMut(env_sid),
            CallableKind::FnOnce => Type::Struct(env_sid),
        };
        let mut params = Vec::with_capacity(1 + def.params.len());
        params.push((0, env_param_ty));
        for (i, &pty) in def.params.iter().enumerate() {
            params.push((1 + i, pty));
        }

        let body = if def.mut_captures.is_empty() {
            let mut call_args = Vec::with_capacity(fields.len() + def.params.len());
            let env_var = Expr::Variable(0, env_param_ty, 0, 0);
            for (idx, (fname, fty)) in fields.iter().enumerate() {
                let field_expr = Expr::Field(Box::new(env_var.clone()), fname.clone(), *fty);
                if idx >= def.value_captures.len() + def.mut_captures.len() {
                    let hidden_ty = def.borrow_captures[idx - def.value_captures.len() - def.mut_captures.len()].1;
                    if hidden_ty == Type::StrView && *fty == Type::String {
                        call_args.push(Argument::View(field_expr));
                    } else if matches!(hidden_ty, Type::VectorRef(_) | Type::StructRef(_)) && !matches!(fty, Type::VectorRef(_) | Type::StructRef(_)) {
                        call_args.push(Argument::Value(Expr::BorrowAs(Box::new(field_expr), false, 0, 0)));
                    } else {
                        call_args.push(Argument::Value(field_expr));
                    }
                } else {
                    call_args.push(Argument::Value(field_expr));
                }
            }
            for (i, &pty) in def.params.iter().enumerate() {
                call_args.push(Argument::Value(Expr::Variable(1 + i, pty, 0, 0)));
            }
            let call_expr = Expr::Call(def.fn_id, call_args, def.ret);
            vec![Statement::Return(call_expr, Vec::new())]
        } else {
            let orig = self.pending.iter().find(|f| f.id == def.fn_id)
                .cloned()
                .ok_or_else(|| self.error_at(at_offset, "internal: missing closure body"))?;
            let cap_count = fields.len();
            let mut slot_to_field: HashMap<usize, (String, Type)> = HashMap::new();
            for (i, (fname, fty)) in fields.iter().enumerate() {
                slot_to_field.insert(orig.params[i].0, (fname.clone(), *fty));
            }
            let mut param_remap: HashMap<usize, usize> = HashMap::new();
            for (i, &(slot, _)) in orig.params.iter().enumerate().skip(cap_count) {
                param_remap.insert(slot, 1 + (i - cap_count));
            }
            let mut stmts = orig.body;
            rewrite_mut_closure_stmts(&mut stmts, env_param_ty, &slot_to_field, &param_remap);
            for p in params.iter_mut().skip(1 + def.params.len()) {
                p.0 += 1;
            }
            stmts
        };

        self.pending.push(Function {
            id: wrapper_fn_id,
            params,
            returns: def.ret,
            body,
            is_main: false,
        });
        let sig = self.ctx.intern_fn_sig(def.params, def.ret);
        Ok(ClosureEnvInfo { env_sid, wrapper_fn_id, kind: def.kind, sig })
    }
}

fn rewrite_mut_closure_stmts(
    stmts: &mut [Statement],
    env_ty: Type,
    slot_to_field: &HashMap<usize, (String, Type)>,
    param_remap: &HashMap<usize, usize>,
) {
    for stmt in stmts {
        match stmt {
            Statement::Declare(slot, expr) => {
                *slot += 1;
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
            }
            Statement::Assign(slot, expr, _) => {
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
                if let Some((fname, fty)) = slot_to_field.get(slot) {
                    *stmt = Statement::FieldStore(0, vec![(fname.clone(), *fty)], *fty, expr.clone(), env_ty);
                } else if let Some(&new_slot) = param_remap.get(slot) {
                    *slot = new_slot;
                } else {
                    *slot += 1;
                }
            }
            Statement::FieldStore(slot, _, _, expr, _) => {
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
                if let Some(&new_slot) = param_remap.get(slot) {
                    *slot = new_slot;
                } else if !slot_to_field.contains_key(slot) {
                    *slot += 1;
                }
            }
            Statement::Print(Printed::Expr(expr)) => {
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
            }
            Statement::Assert(expr, ..) => {
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
            }
            Statement::Return(expr, frees) => {
                rewrite_mut_closure_expr(expr, env_ty, slot_to_field, param_remap);
                frees.retain(|(s, _)| !slot_to_field.contains_key(s));
                for (s, _) in frees.iter_mut() {
                    if let Some(&new_slot) = param_remap.get(s) { *s = new_slot; } else { *s += 1; }
                }
            }
            Statement::Frees(frees) => {
                frees.retain(|(s, _)| !slot_to_field.contains_key(s));
                for (s, _) in frees.iter_mut() {
                    if let Some(&new_slot) = param_remap.get(s) { *s = new_slot; } else { *s += 1; }
                }
            }
            Statement::If(cond, yes, no) => {
                rewrite_mut_closure_expr(cond, env_ty, slot_to_field, param_remap);
                rewrite_mut_closure_stmts(yes, env_ty, slot_to_field, param_remap);
                rewrite_mut_closure_stmts(no, env_ty, slot_to_field, param_remap);
            }
            Statement::While(cond, body) => {
                rewrite_mut_closure_expr(cond, env_ty, slot_to_field, param_remap);
                rewrite_mut_closure_stmts(body, env_ty, slot_to_field, param_remap);
            }
            _ => {}
        }
    }
}

fn rewrite_mut_closure_expr(
    expr: &mut Expr,
    env_ty: Type,
    slot_to_field: &HashMap<usize, (String, Type)>,
    param_remap: &HashMap<usize, usize>,
) {
    match expr {
        Expr::Variable(slot, _ty, line, col) => {
            if let Some((fname, fty)) = slot_to_field.get(slot) {
                *expr = Expr::Field(Box::new(Expr::Variable(0, env_ty, *line, *col)), fname.clone(), *fty);
            } else if let Some(&new_slot) = param_remap.get(slot) {
                *slot = new_slot;
            } else {
                *slot += 1;
            }
        }
        Expr::Binary(a, _, b, ..) => {
            rewrite_mut_closure_expr(a, env_ty, slot_to_field, param_remap);
            rewrite_mut_closure_expr(b, env_ty, slot_to_field, param_remap);
        }
        Expr::Neg(a, ..) | Expr::Not(a) | Expr::Field(a, ..) | Expr::ScalarCast(a, ..) => {
            rewrite_mut_closure_expr(a, env_ty, slot_to_field, param_remap);
        }
        Expr::Call(_, args, _) | Expr::FnCall(_, args, _, _) => {
            for arg in args {
                if let Argument::Value(e) | Argument::View(e) = arg {
                    rewrite_mut_closure_expr(e, env_ty, slot_to_field, param_remap);
                }
            }
        }
        Expr::FnExprCall(callee, args, ..) => {
            rewrite_mut_closure_expr(callee, env_ty, slot_to_field, param_remap);
            for arg in args {
                if let Argument::Value(e) | Argument::View(e) = arg {
                    rewrite_mut_closure_expr(e, env_ty, slot_to_field, param_remap);
                }
            }
        }
        Expr::StructLit(_, vals) | Expr::EnumLit(_, _, _, vals) | Expr::VecLit(vals, ..) => {
            for v in vals {
                rewrite_mut_closure_expr(v, env_ty, slot_to_field, param_remap);
            }
        }
        _ => {}
    }
}
