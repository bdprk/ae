//! Parser and single-pass semantic checker: tokens to typed statements.
use super::*;
use super::route_marks::*;

#[derive(Default)]
pub(super) struct SharedExpressionEffects {
    pub(super) moves: Vec<usize>,
    pub(super) call_borrows: Vec<usize>,
    pub(super) mut_borrows: Vec<usize>,
    pub(super) conflicts: Vec<(usize, usize, String, &'static str, Option<String>)>,
}

/// The remedy attached to a use-after-move diagnostic. The move is not tracked
/// to a byte range, so this names both available fixes instead of pointing at a
/// site that may not exist.
pub(super) fn moved_value_hint(diagnostic: ae_span::Diagnostic) -> ae_span::Diagnostic {
    diagnostic.with_suggestion("Owned values move on assignment or when passed by value; read what you need before the move, or build a fresh value.")
}

/// The remedy for `s = f(s)`, where the assignment target is consumed by the
/// call on its own right-hand side.
pub(super) fn moved_self_assignment_hint(diagnostic: ae_span::Diagnostic) -> ae_span::Diagnostic {
    diagnostic.with_suggestion("This call consumes the value being assigned to; bind the result to a new name, or read its fields into locals first.")
}

impl<'a> Parser<'a> {
    pub(super) fn current(&self) -> &Kind { &self.tokens[self.pos].kind }
    /// A lowering site that still hands an expression to the typed Pratt
    /// parser instead of lowering the shared syntax node. With
    /// `AE_ROUTE_REPORT` set it names its token, so the dual-parser gap is
    /// measurable and a ratchet can drive it to zero.
    /// Whether a propagation operand is a shape the shared route can lower:
    /// a direct call (marked by the scan), a pure builtin call, or a variant
    /// construction. Anything else keeps the typed token parse.
    pub(super) fn shared_propagation_operand(&self, operand: &ae_syntax::expression::Expression) -> bool {
        use ae_syntax::expression::ExpressionKind;
        let ExpressionKind::Call { callee, .. } = &operand.kind else { return false };
        match &callee.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => {
                self.resolved_expressions().direct_calls.contains(&operand.range.start)
                    || (resolved_builtin(name) && !typed_intrinsic(name))
            }
            ExpressionKind::Qualified { .. } => self.resolved_expressions().enum_constructors.contains(&operand.range.start),
            _ => false,
        }
    }

    /// The slot a resolved definition names, or the typed route's unknown-
    /// variable diagnostic when a permissive scan handed the lowering an id
    /// the current scope does not bind. A mutant gets a diagnostic, never a
    /// panic.
    pub(super) fn resolved_slot(
        &self,
        def: Option<&ae_resolve::DefId>,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
        offset: usize,
        name: &str,
    ) -> Result<(usize, Type), ae_span::Diagnostic> {
        let def = self.resolved_definition(def, offset, name)?;
        slots.get(&def).copied()
            .ok_or_else(|| self.error_at(offset, &format!("unknown variable '{name}'")).with_code("E0101"))
    }

    pub(super) fn resolved_definition(
        &self,
        def: Option<&ae_resolve::DefId>,
        offset: usize,
        name: &str,
    ) -> Result<ae_resolve::DefId, ae_span::Diagnostic> {
        def.copied()
            .ok_or_else(|| self.error_at(offset, &format!("unknown variable '{name}'")).with_code("E0101"))
    }

    pub(super) fn fallback_expression(&mut self, min: u8, moving: bool) -> Result<(Expr, Type), ae_span::Diagnostic> {
        if std::env::var_os("AE_STRICT_ROUTE").is_some() {
            return Err(self.fail("this expression is not supported by the body route"));
        }
        if std::env::var_os("AE_ROUTE_REPORT").is_some() {
            let offset = self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0);
            eprintln!("route typed-expression at {offset}");
        }
        self.expression(min, moving)
    }

    pub(super) fn fail(&self, message: &str) -> ae_span::Diagnostic {
        let token = &self.tokens[self.pos];
        ae_syntax::diagnostic_range(&self.file, self.source, token.offset..token.end, message)
    }
    // The display form of one instantiation in the recursion trace (M1.3.06):
    // names only, rendered never compared — cycles compare the interned key.
    pub(super) fn instantiation_display(&self, name: &str, args: &[Type]) -> String {
        if args.is_empty() { return name.to_owned(); }
        format!("{}[{}]", name, args.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", "))
    }
    pub(super) fn instantiation_trace(&self, current: &str) -> String {
        let mut chain: Vec<&str> = self.generics.instantiation_trace.iter().map(|(_, _, display)| display.as_str()).collect();
        chain.push(current);
        chain.join(" -> ")
    }
    pub(super) fn error_at(&self, offset: usize, message: &str) -> ae_span::Diagnostic {
        let end = self.tokens.binary_search_by_key(&offset, |token| token.offset)
            .ok().map_or(offset, |index| self.tokens[index].end);
        ae_syntax::diagnostic_range(&self.file, self.source, offset..end, message)
    }
    pub(super) fn binding_label(&self, diagnostic: ae_span::Diagnostic, id: usize) -> ae_span::Diagnostic {
        let Some(range) = self.binding_ranges.get(&id) else { return diagnostic };
        let source = ae_span::SourceFile { name: Some(&self.file), text: self.source };
        match source.span(range.clone()) {
            Some(span) => diagnostic.with_secondary(span, "binding declared here"),
            None => diagnostic,
        }
    }
    pub(super) fn immutable_binding_error(&self, diagnostic: ae_span::Diagnostic, id: usize) -> ae_span::Diagnostic {
        let diagnostic = self.binding_label(diagnostic, id);
        if diagnostic.secondary.is_empty() { diagnostic } else {
            diagnostic.with_suggestion("If mutation is intended, declare the binding with `var`.")
        }
    }
    pub(super) fn duplicate_binding_error(&self, name: &str) -> ae_span::Diagnostic {
        self.duplicate_binding_error_at(self.tokens[self.pos].offset, name)
    }

    // The tree-driven callers know the offending token's range already, so
    // they name it directly instead of relying on the moving cursor.
    pub(super) fn duplicate_binding_error_at(&self, at: usize, name: &str) -> ae_span::Diagnostic {
        let diagnostic = self.error_at(at, &format!("duplicate variable '{name}'")).with_code("E0104");
        match self.names.get(name) {
            Some(&(id, _, _)) => self.binding_label(diagnostic, id),
            None => diagnostic,
        }
    }
    pub(super) fn take(&mut self, expected: Kind) -> Result<(), ae_span::Diagnostic> {
        ae_syntax::expect_token(&self.file, self.source, &self.tokens[self.pos], expected)?;
        self.pos += 1; Ok(())
    }
    pub(super) fn identifier(&mut self) -> Result<String, ae_span::Diagnostic> {
        let name = ae_syntax::parse_identifier(&self.file, self.source, &self.tokens[self.pos])?;
        self.pos += 1;
        Ok(name.to_owned())
    }
    // Whether an `unsafe` block's body contains any operation the block
    // exists to legalize: a raw dereference, a pointer store, or a call to
    // a declared extern / `unsafe fn`. Nested `unsafe` blocks check their
    // own bodies — the outer block's warning is about ITS body. W0702
    // (spec/unsafe.md U4): a block with none is silent decoration.
    pub(super) fn block_has_unchecked_operation(&self, statements: &[Statement]) -> bool {
        for statement in statements {
            if self.statement_has_unchecked_operation(statement) { return true; }
        }
        false
    }

    pub(super) fn statement_has_unchecked_operation(&self, statement: &Statement) -> bool {
        match statement {
            Statement::Unsafe(body) => self.block_has_unchecked_operation(body),
            Statement::PtrStore(..) => true,
            Statement::If(cond, yes, no) => {
                self.expr_has_unchecked_operation(cond)
                    || self.block_has_unchecked_operation(yes)
                    || self.block_has_unchecked_operation(no)
            }
            Statement::While(cond, body) => {
                self.expr_has_unchecked_operation(cond) || self.block_has_unchecked_operation(body)
            }
            Statement::Return(expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Exit(expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Print(printed) => match printed {
                Printed::Expr(e) => self.expr_has_unchecked_operation(e),
                Printed::Text(_) => false,
            },
            Statement::Assert(cond, ..) => self.expr_has_unchecked_operation(cond),
            Statement::Assign(_, expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Declare(_, expr) => self.expr_has_unchecked_operation(expr),
            Statement::For(_, _, body, ..) | Statement::ForTokens(_, _, body, ..) => {
                self.block_has_unchecked_operation(body)
            }
            Statement::Match(_, _, arms) => arms.iter().any(|arm| self.block_has_unchecked_operation(&arm.body)),
            _ => false,
        }
    }

    pub(super) fn expr_has_unchecked_operation(&self, expr: &Expr) -> bool {
        match expr {
            Expr::DerefPtr(..) => true,
            // Pointer arithmetic is the same class of unchecked operation
            // (it gates on unsafe_depth exactly like the deref).
            Expr::PtrOffset(..) | Expr::PtrDropInPlace(..) | Expr::PtrWrite(..) => true,
            Expr::Call(id, args, _) => {
                // A call to a declared `extern "C" fn` or an `unsafe fn` is
                // exactly what the block legalizes (the same test E0701
                // applies at the call site). Both registries fill during the
                // signature pass, which completes before any body parses, so
                // the id-keyed lookup is order-safe here — including a call
                // that textually precedes its declaration. `Expr::FnCall` is
                // a call through a `fn(T) -> R` pointer whose id comes from
                // the closure-signature interner, a different id space, and
                // closures are never unsafe — no registry check there.
                if self.ctx.is_unsafe_fn(*id) || self.ctx.is_extern_fn(*id) {
                    return true;
                }
                args.iter().any(|arg| match arg {
                    Argument::Value(e) | Argument::View(e) => self.expr_has_unchecked_operation(e),
                    _ => false,
                })
            }
            Expr::FnCall(_, args, ..) => {
                args.iter().any(|arg| match arg {
                    Argument::Value(e) | Argument::View(e) => self.expr_has_unchecked_operation(e),
                    _ => false,
                })
            }
            Expr::Binary(a, _, b, ..) => self.expr_has_unchecked_operation(a) || self.expr_has_unchecked_operation(b),
            Expr::Field(base, ..) => self.expr_has_unchecked_operation(base),
            Expr::Variable(..) | Expr::Number(..) | Expr::FloatLit(..) | Expr::BoolLit(..) | Expr::StrLit(..) => false,
            _ => false,
        }
    }

    pub(super) fn position(&self) -> (usize, usize) {
        let token = &self.tokens[self.pos];
        (token.line, token.column)
    }
    // Prelude result types are always registered, so naming one cannot fail.
    pub(super) fn prelude_type(&self, name: &str) -> Type {
        match self.enum_names.get(name) {
            Some(&id) => Type::Enum(self.enums[id].name, id),
            None => unreachable!("prelude enum {name} is registered for every program"),
        }
    }

    // Is any borrow of this slot live? Two registers feed the answer:
    // `borrows` holds the persistent borrows (views, token cursors, the
    // containers of for-in loops, match subjects) that last until their
    // scope or loop ends, and `call_borrows` holds the borrows a call
    // argument created, which die when the statement that made the call
    // ends -- the callee has returned by then and nothing retains the
    // pointer (ADR-0184).
    // Whether every borrow on `id` is a view-kind borrow: only then can a
    // write conflict be deferred to the NLL adjudication (the view dies
    // after its last use; container borrows run to the scope's end).
    pub(super) fn all_borrows_are_views(&self, id: usize) -> bool {
        self.is_borrowed(id) && !self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::Container)
    }

    // Whether the only active borrows on `id` are persistent view borrows:
    // the deferral precondition for a mut lending call (M2.5). Call-argument
    // borrows and earlier mut lends in the same statement are statement-
    // scoped exclusions the on-sight rule still guards (`c.doubled() +
    // c.inc()` stays rejected), and container borrows keep their scope rule.
    pub(super) fn only_view_borrows_active(&self, id: usize) -> bool {
        !self.call_borrows.iter().any(|&(bid, _)| bid == id)
            && !self.mut_borrows.iter().any(|&(bid, _)| bid == id)
            && self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::View)
            && !self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::Container)
    }

    pub(super) fn is_borrowed(&self, id: usize) -> bool {
        self.borrows.iter().any(|&(bid, _, _)| bid == id)
            || self.call_borrows.iter().any(|&(bid, _)| bid == id)
            || self.mut_borrows.iter().any(|&(bid, _)| bid == id)
    }

    // Adjudicate the write conflicts this function deferred (M2.5 L1): the
    // graph decides whether a view of the written slot may still be read.
    // A surviving conflict is the positioned rejection; everything else was
    // a write after the view's last use, which now parses.
    // Whether the graph's release scan will see a call through this
    // signature id: the scan reads the four tables every `Expr::Call` id
    // can come from. A deferral whose callee is outside them would be an
    // unsound allowance, so the deferral sites reject on sight instead.
    pub(super) fn signature_in_release_tables(&self, callee: Option<usize>) -> bool {
        let Some(id) = callee else { return false; };
        self.functions.values().any(|s| s.id == id)
            || self.generics.inherent.values().any(|m| m.signature.id == id)
            || self.generics.fn_done.values().any(|(iid, _, _)| *iid == id)
            || self.generics.generic_impl_done.values().any(|(iid, _, _)| *iid == id)
    }

    /// The inherent (non-generic) method a receiver type names, if the call
    /// is one the resolved route can lower: a borrowed receiver, a return
    /// the route admits, and value parameters only. The call gate and the
    /// lowering share this, so the route admits exactly what it can emit.
    /// Generic-instance methods need `instantiate_impl_method` (a `&mut self`
    /// operation) and stay on the typed route.
    pub(super) fn resolve_inherent_method(&self, method_name: &str, base_ty: Type) -> Option<(Signature, Option<Type>, Type)> {
        let type_name = match base_ty {
            Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => self.structs.get(sid)?.name.clone(),
            Type::Enum(name, _) => name.to_owned(),
            _ => return None,
        };
        let definition = self.generics.inherent.get(&format!("{type_name}#{method_name}"))?;
        // Any receiver kind is admitted: borrowed receivers keep their
        // bookkeeping and a by-value receiver moves (enum receivers are by
        // value only). A receiver-less associated function is not a method
        // call through a value.
        let receiver = definition.receiver;
        receiver?;
        let signature = definition.signature.clone();
        let ret = signature.ret;
        let ret_ok = scalar_type(ret) || matches!(ret, Type::String | Type::Enum(..))
            || owned_plain_struct(ret, self.structs) || owned_field_struct(ret, self.structs);
        // `params[0]` is the receiver; the value parameters follow it.
        let params_ok = signature.params.iter().skip(1).copied()
            .all(|ty| scalar_type(ty) || matches!(ty, Type::Struct(_) | Type::String));
        if !ret_ok || !params_ok {
            return None;
        }
        Some((signature, receiver, base_ty))
    }

    /// Whether the inherent table declares `method` on this receiver type at
    /// all, receiver-less entries included. The call gate uses it to admit a
    /// method the route's receiver rules refuse (an associated function called
    /// through a value); the lowering then reports the typed route's own
    /// diagnostic instead of a shape rejection.
    pub(super) fn inherent_entry_exists(&self, method: &str, base_ty: Type) -> bool {
        let type_name = match base_ty {
            Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => self.structs.get(sid).map(|defn| defn.name.clone()),
            Type::Enum(name, _) => Some(name.to_owned()),
            _ => None,
        };
        type_name.is_some_and(|name| self.generics.inherent.contains_key(&format!("{name}#{method}")))
    }


    /// Whether any declaration answers `name` as a callable — a function
    /// (file-local, imported, or an `extern`/`unsafe fn`), a generic function,
    /// a trait method of a constrained parameter, or a reserved builtin.
    /// Visibility and signature shape stay the call gate's business; this
    /// answers identity alone, so an unknown call can reach the lowering's
    /// `resolve_trait_call` (the typed route's E0207).
    pub(super) fn call_has_signature(&self, name: &str) -> bool {
        if RESERVED.contains(&name) { return true; }
        // Intrinsics the typed expression parser dispatches by name and live
        // in neither table: a call to one is known.
        if typed_intrinsic(name) { return true; }
        if self.functions.contains_key(name) || self.generics.fn_names.contains_key(name) { return true; }
        self.trait_bounds.values().flatten().any(|&tid| {
            self.generics.traits.get(tid).is_some_and(|defn| defn.methods.iter().any(|method| method.name == name))
        })
    }

    pub(super) fn adjudicate_deferred_conflicts(&mut self, function: &Function) -> Result<(), ae_span::Diagnostic> {
        let mut_sigs = borrowscope::mut_borrow_positions(self.functions, self.generics);
        let outcome = borrowscope::adjudicate_deferred(function, &self.deferred_write_conflicts, &mut_sigs, self.ctx);
        match outcome {
            Ok(()) => { self.deferred_write_conflicts.clear(); }
            Err(message) => {
                let (_, offset, _, _, _) = self.deferred_write_conflicts[0];
                return Err(self.error_at(offset, &message));
            }
        }
        // ADR-0215: the deferred spawn-escape proofs run on the same graph.
        self.adjudicate_borrow_spawns(function)?;
        Ok(())
    }
    // ADR-0215: the deferred spawn-escape proofs (closure id, the borrowed
    // variable ids, the spawn's byte offset).
    pub(super) fn adjudicate_borrow_spawns(&mut self, function: &Function) -> Result<(), ae_span::Diagnostic> {
        let outcome = borrowscope::adjudicate_borrow_spawns(function, &self.deferred_borrow_spawns, self.ctx);
        match outcome {
            Ok(()) => { self.deferred_borrow_spawns.clear(); Ok(()) }
            Err(message) => {
                let (_, _, offset) = self.deferred_borrow_spawns[0];
                Err(self.error_at(offset, &message))
            }
        }
    }
    // Views borrow their base: element reads and slice_views of variable
    // bases register the owner so it cannot be modified or moved while a
    // view can still be read.
    pub(super) fn record_borrow_bases(&mut self, e: &Expr) {
        match e {
            Expr::StrVecIndex(id, idx, _, _, _) => {
                if !self.is_borrowed(*id) {
                    self.borrows.push((*id, self.scopes.len(), BorrowKind::Container));
                }
                self.record_borrow_bases(idx);
            }
            Expr::SliceView(_, _, a, b, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::StructLit(_, values) => { for v in values { self.record_borrow_bases(v); } }
            Expr::EnumLit(_, _, _, values) => { for v in values { self.record_borrow_bases(v); } }
            Expr::Binary(a, _, b, _, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::Neg(a, _, _) | Expr::Not(a) | Expr::Len(a, _, _) => self.record_borrow_bases(a),
            Expr::Index(_, _, a, _, _) => self.record_borrow_bases(a),
            Expr::VecIndex(_, a, _, _, _, _) => self.record_borrow_bases(a),
            Expr::Call(_, args, _) => {
                for arg in args {
                    match arg {
                        Argument::Value(e) => self.record_borrow_bases(e),
                        Argument::View(e) => self.record_borrow_bases(e),
                        Argument::Borrow(_, _) | Argument::FnPtr(_) => {}
                    }
                }
            }
            Expr::Contains(a, b, _, _) | Expr::Split(a, b, _, _) | Expr::StrEq(a, b, _) | Expr::Find(a, b, _, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::WriteFile(a, b, _, _) | Expr::Slice(_, a, b, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::MapGet(_, key, _, _, _) | Expr::MapHas(_, key, _, _) | Expr::MapRemove(_, key, _, _) => self.record_borrow_bases(key),
            Expr::Join(_, sep, _, _) => self.record_borrow_bases(sep),
            Expr::Field(base, _, _) => self.record_borrow_bases(base),
            _ => {}
        }
    }
    pub(super) fn live_owned(&self) -> Vec<(usize, Type)> {
        // Parameters are owned by the callee but live outside the block
        // scopes; a `return` must release them exactly like a local unless
        // the body moved them out (the drops pass trims by `moved`). A slot
        // present in both lists (the receiver, which the entry scope already
        // holds) is released once.
        let mut seen = std::collections::HashSet::new();
        self.scopes.iter().flatten().copied()
            .chain(self.owned_params.iter().copied())
            .filter(|(id, ty)| ty.owned() && seen.insert(*id))
            .collect()
    }
    pub(super) fn expression_index_for(&self, file: &str, tokens: &Rc<Vec<Token>>) -> Rc<OnceCell<ResolvedExpressions>> {
        if self.file == file && Rc::ptr_eq(&self.tokens, tokens) {
            Rc::clone(&self.resolved_expressions)
        } else {
            Rc::default()
        }
    }

    // Planning aid for the P1.1 slices: with AE_ROUTE_REASONS=1 the first
    // statement that keeps a body off the route is printed with its source
    // line. A compound statement fails because one of the statements inside
    // it fails, so the walk descends into blocks and reports the innermost
    // failing statement — the shape to support next.
    pub(super) fn report_route_reason(&self, label: &str, statement: &ae_syntax::statement::Statement, fails: &dyn Fn(&ae_syntax::statement::Statement) -> bool) {
        if std::env::var_os("AE_ROUTE_REASONS").is_none() {
            return;
        }
        use ae_syntax::statement::{ElseBranch, StatementKind};
        let mut current = statement;
        loop {
            let nested: Option<&ae_syntax::statement::Statement> = match &current.kind {
                StatementKind::If { then_body, else_body, .. } => then_body.iter().find(|s| fails(s)).or_else(|| match else_body {
                    Some(ElseBranch::Block(body)) => body.iter().find(|s| fails(s)),
                    Some(ElseBranch::If(branch)) => fails(branch).then_some(branch.as_ref()),
                    None => None,
                }),
                StatementKind::While { body, .. } | StatementKind::ForRange { body, .. } | StatementKind::ForIn { body, .. } => {
                    body.iter().find(|s| fails(s))
                }
                StatementKind::Match { arms, .. } => arms.iter().flat_map(|arm| arm.body.iter()).find(|s| fails(s)),
                StatementKind::Unsafe(body) => body.iter().find(|s| fails(s)),
                _ => None,
            };
            match nested {
                Some(next) => current = next,
                None => break,
            }
        }
        let text = self.source.get(current.range.clone()).unwrap_or("").lines().next().unwrap_or("").trim();
        let line = 1 + self.source[..current.range.start.min(self.source.len())].matches('\n').count();
        eprintln!("route reason {label} {}:{line}: {}", self.file, text.chars().take(80).collect::<String>());
    }

    pub(super) fn resolved_expressions(&self) -> &ResolvedExpressions {
        self.resolved_expressions.get_or_init(|| self.build_resolved_expressions())
    }


























































}
