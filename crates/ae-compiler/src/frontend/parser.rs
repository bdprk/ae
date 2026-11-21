//! Parser and single-pass semantic checker: tokens to typed statements.
use super::*;

#[derive(Default)]
struct SharedExpressionEffects {
    moves: Vec<usize>,
    call_borrows: Vec<usize>,
    mut_borrows: Vec<usize>,
    conflicts: Vec<(usize, usize, String, &'static str, Option<String>)>,
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

fn scalar_type(ty: Type) -> bool {
    matches!(ty, Type::Int | Type::F64 | Type::Bool)
}

/// The registry struct a field expression reads from: a parameter's
/// immutable borrowed spelling names a file-local struct item; a nested
/// field's struct is the declared type of the field it reads. Generic
/// instances and imported structs have no registry entry here and stay on
/// the typed route.
fn field_owner_struct(
    base: &ae_syntax::expression::Expression,
    scalar_reads: &HashMap<usize, ae_resolve::DefId>,
    struct_definitions: &HashSet<ae_resolve::DefId>,
    resolution: &ae_resolve::resolve::Resolution,
    struct_names: &HashMap<String, usize>,
    structs: &[StructDef],
) -> Option<usize> {
    match &base.kind {
        ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)) => {
            let def = scalar_reads.get(&base.range.start)?;
            if !struct_definitions.contains(def) {
                return None;
            }
            let entry = resolution.defs.get(def.0 as usize)?;
            let item = definition_struct_item(entry)?;
            let item_entry = resolution.defs.get(item.0 as usize)?;
            struct_names.get(&item_entry.name).copied()
        }
        ae_syntax::expression::ExpressionKind::Field { base: inner_base, name } => {
            let sid = field_owner_struct(inner_base, scalar_reads, struct_definitions, resolution, struct_names, structs)?;
            let fty = field_ty_in(sid, name, structs)?;
            match fty {
                Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => Some(sid),
                _ => None,
            }
        }
        _ => None,
    }
}

fn field_ty_in(sid: usize, field: &str, structs: &[StructDef]) -> Option<Type> {
    let defn = structs.get(sid)?;
    defn.fields.iter().find(|(name, _)| name == field).map(|(_, ty)| *ty)
}

/// The struct item an immutable borrowed-struct spelling names. Only an
/// immutable borrow qualifies: by-value and `&mut` receivers carry
/// ownership effects (moves, drop at scope exit) that stay on the typed
/// route.
fn definition_struct_item(entry: &ae_resolve::DefEntry) -> Option<ae_resolve::DefId> {
    match &entry.inferred {
        Some(ae_resolve::TypeSpelling::Borrow { mutable: false, inner }) => match inner.as_ref() {
            ae_resolve::TypeSpelling::Named { def } => Some(*def),
            _ => None,
        },
        _ => None,
    }
}

/// Every expression in the tree rooted at `expression`. `statement.expressions()`
/// and `visit_expressions` stop at the statement's top-level values; the body
/// route's checks need the nested ones too. Closure bodies stay opaque (the
/// compiler owns capture inference), so their contents are not visited.
fn visit_expression_tree(expression: &ae_syntax::expression::Expression, visit: &mut impl FnMut(&ae_syntax::expression::Expression)) {
    use ae_syntax::expression::ExpressionKind;
    visit(expression);
    match &expression.kind {
        ExpressionKind::Group(inner) | ExpressionKind::Propagate(inner) | ExpressionKind::Prefix { operand: inner, .. } => {
            visit_expression_tree(inner, visit);
        }
        ExpressionKind::PointerCast { base, .. } => visit_expression_tree(base, visit),
        ExpressionKind::Binary { left, right, .. } => {
            visit_expression_tree(left, visit);
            visit_expression_tree(right, visit);
        }
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            elements.iter().for_each(|element| visit_expression_tree(element, visit));
        }
        ExpressionKind::Struct { target, fields } => {
            visit_expression_tree(target, visit);
            fields.iter().for_each(|field| visit_expression_tree(&field.value, visit));
        }
        ExpressionKind::Call { callee, arguments } => {
            visit_expression_tree(callee, visit);
            arguments.iter().for_each(|argument| visit_expression_tree(argument, visit));
        }
        ExpressionKind::Index { base, index } => {
            visit_expression_tree(base, visit);
            visit_expression_tree(index, visit);
        }
        ExpressionKind::Field { base, .. } | ExpressionKind::Specialization { base, .. }
        | ExpressionKind::BracketedCallTarget { base, .. } | ExpressionKind::Qualified { base, .. } => {
            visit_expression_tree(base, visit);
        }
        _ => {}
    }
}

fn scalar_body_expression(expression: &ae_syntax::expression::Expression, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool) -> bool {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Atom(Kind::Ident(_)) => resolved.scalar_reads.get(&expression.range.start)
            .is_some_and(|id| !resolved.array_definitions.contains(id) && !resolved.struct_definitions.contains(id)),
        // A struct parameter's field read, possibly nested (`o.inner.x`):
        // the chain's base may be a scalar read or a struct-typed chain
        // receiver, but only a scalar field lowers as a value.
        ExpressionKind::Field { base, .. } => {
            let rooted = match &base.kind {
                ExpressionKind::Atom(Kind::Ident(_)) => true,
                ExpressionKind::Field { .. } => resolved.struct_field_reads.contains(&base.range.start)
                    || resolved.struct_field_chains.contains(&base.range.start),
                _ => false,
            };
            rooted && resolved.struct_field_reads.contains(&expression.range.start)
        }
        ExpressionKind::Index { base, index } => {
            matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.get(&base.range.start)
                    .is_some_and(|id| resolved.array_definitions.contains(id))
                && scalar_body_expression(index, resolved, supports_call)
        }
        ExpressionKind::Atom(Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False) => true,
        ExpressionKind::Group(inner) | ExpressionKind::Prefix { operand: inner, .. } => {
            scalar_body_expression(inner, resolved, supports_call)
        }
        ExpressionKind::Binary { left, right, .. } => {
            scalar_body_expression(left, resolved, supports_call) && scalar_body_expression(right, resolved, supports_call)
        }
        ExpressionKind::Call { callee, arguments } => {
            matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name))
                if (resolved.direct_calls.contains(&callee.range.start)
                    || resolved.unresolved_calls.contains(&callee.range.start))
                    && supports_call(name, callee.range.start))
                && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call))
        }
        _ => false,
    }
}

fn compound_assignment_operator(operator: &Kind) -> Option<Kind> {
    match operator {
        Kind::PlusEqual => Some(Kind::Plus),
        Kind::MinusEqual => Some(Kind::Minus),
        Kind::StarEqual => Some(Kind::Star),
        Kind::SlashEqual => Some(Kind::Slash),
        Kind::PercentEqual => Some(Kind::Percent),
        _ => None,
    }
}

fn scalar_body_statement(statement: &ae_syntax::statement::Statement, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool) -> bool {
    use ae_syntax::statement::{ElseBranch, StatementKind};
    match &statement.kind {
        StatementKind::Binding { name_range, initializer, .. } => {
            resolved.scalar_definitions.contains_key(&name_range.start)
                && match &initializer.kind {
                    ae_syntax::expression::ExpressionKind::Array(elements) => elements.iter()
                        .all(|element| scalar_body_expression(element, resolved, supports_call)),
                    _ => scalar_body_expression(initializer, resolved, supports_call),
                }
        }
        StatementKind::Assign { target, operator, value } => {
            match &target.kind {
                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)) => {
                    resolved.scalar_writes.get(&target.range.start)
                        .is_some_and(|id| !resolved.array_definitions.contains(id))
                        && scalar_body_expression(value, resolved, supports_call)
                }
                // `a[i] = v` stores through a resolved array binding. Only
                // the plain operator: a compound store stays on the typed
                // route, which rejects it for arrays.
                ae_syntax::expression::ExpressionKind::Index { base, index }
                    if *operator == Kind::Equal =>
                {
                    matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                        && resolved.scalar_writes.get(&base.range.start)
                            .is_some_and(|id| resolved.array_definitions.contains(id))
                        && scalar_body_expression(index, resolved, supports_call)
                        && scalar_body_expression(value, resolved, supports_call)
                }
                _ => false,
            }
        }
        StatementKind::Return(value) | StatementKind::Print(value) | StatementKind::Assert(value) => {
            scalar_body_expression(value, resolved, supports_call)
        }
        StatementKind::If { condition, then_body, else_body } => {
            scalar_body_expression(condition, resolved, supports_call)
                && then_body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call))
                && match else_body {
                    Some(ElseBranch::Block(body)) => body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call)),
                    Some(ElseBranch::If(branch)) => scalar_body_statement(branch, resolved, supports_call),
                    None => true,
                }
        }
        StatementKind::While { condition, body } => {
            scalar_body_expression(condition, resolved, supports_call)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call))
        }
        StatementKind::ForRange { binding_range, start, end, body, .. } => {
            resolved.scalar_definitions.contains_key(&binding_range.start)
                && scalar_body_expression(start, resolved, supports_call)
                && scalar_body_expression(end, resolved, supports_call)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call))
        }
        StatementKind::Break | StatementKind::Continue => true,
        _ => false,
    }
}

impl<'a> Parser<'a> {
    pub(super) fn current(&self) -> &Kind { &self.tokens[self.pos].kind }
    pub(super) fn fail(&self, message: &str) -> ae_span::Diagnostic {
        let token = &self.tokens[self.pos];
        ae_syntax::diagnostic_range(&self.file, self.source, token.offset..token.end, message)
    }
    // The display form of one instantiation in the recursion trace (M1.3.06):
    // names only, rendered never compared — cycles compare the interned key.
    pub(super) fn instantiation_display(&self, name: &str, args: &[Type]) -> String {
        if args.is_empty() { return name.to_owned(); }
        format!("{}[{}]", name, args.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "))
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
    fn block_has_unchecked_operation(&self, statements: &[Statement]) -> bool {
        for statement in statements {
            if self.statement_has_unchecked_operation(statement) { return true; }
        }
        false
    }

    fn statement_has_unchecked_operation(&self, statement: &Statement) -> bool {
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

    fn expr_has_unchecked_operation(&self, expr: &Expr) -> bool {
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
                if crate::frontend::is_unsafe_fn(*id) || crate::frontend::is_extern_fn(*id) {
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
    #[inline(never)]
    fn concurrency_stmt(&mut self, name: &str) -> Result<Statement, ae_span::Diagnostic> {
        let (line, col) = self.position();
        let at = self.tokens[self.pos].offset;
        self.pos += 2;
        if name == "task_yield" {
            self.take(Kind::RParen)?;
            self.take(Kind::Semi)?;
            return Ok(Statement::TaskYield);
        }
        if matches!(name, "task_spawn" | "scope_spawn" | "pool_spawn") {
            let (ex, ex_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
                Some(pair) => pair,
                None => self.expression(0, false)?,
            };
            let expected_ty = match name {
                "task_spawn" => Type::Exec,
                "scope_spawn" => Type::TaskScope,
                _ => Type::Pool,
            };
            if ex_ty != expected_ty {
                let art = if name == "task_spawn" { "an" } else { "a" };
                return Err(self.fail(&format!("{name} expects {art} {} handle as its first argument", expected_ty.name())));
            }
            self.take(Kind::Comma)?;
            let (callable, callable_ty) = self.expression(0, false)?;
            self.take(Kind::RParen)?;
            self.take(Kind::Semi)?;
            let mut slots = Vec::new();
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
                        slots.push(Expr::Variable(*outer_id, *ty, 0, 0));
                    }
                    crate::frontend::closures::record_spawned(crate::frontend::closures::SpawnedThread::Closure {
                        fn_id: crate::frontend::closures::closure_fn_id(*cid),
                        captures: def.value_captures.iter().map(|(_, ty)| *ty).collect(),
                        borrows: Vec::new(),
                        ret: def.ret,
                    });
                    *cid
                }
                _ => return Err(self.fail("task_spawn expects a zero-parameter closure returning i64")),
            };
            return Ok(match name {
                "task_spawn" => Statement::TaskSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
                "scope_spawn" => Statement::ScopeSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
                _ => Statement::PoolSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
            });
        }
        if name == "chan_close" {
            let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                Some(pair) => pair,
                None => self.expression(0, false)?,
            };
            if handle_ty != Type::Chan {
                return Err(self.fail("chan_close expects a chan handle"));
            }
            self.take(Kind::RParen)?;
            self.take(Kind::Semi)?;
            return Ok(Statement::ChanClose(Box::new(handle)));
        }
        let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
            Some(pair) => pair,
            None => self.expression(0, false)?,
        };
        let sid = match handle_ty {
            Type::Shared(sid) => sid,
            _ => return Err(self.fail("shared_set expects a shared handle")),
        };
        self.take(Kind::Comma)?;
        let (value, value_ty) = self.expression(0, false)?;
        let expected_ty = shared_types_get(sid);
        if value_ty != expected_ty {
            return Err(self.fail(&format!("shared_set expects {}, found {}", expected_ty.name(), value_ty.name())));
        }
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        if !self.live_owned().iter().any(|(_, ty)| *ty == Type::Guard) {
            return Err(self.fail("shared_set requires a live mutex guard in scope; call mutex_lock(...) first"));
        }
        Ok(Statement::SharedSet(Box::new(handle), Box::new(value), sid))
    }

    pub(super) fn block(&mut self) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        if self.block_depth >= 64 { return Err(self.fail("block nesting limit exceeded (64)")); }
        self.block_depth += 1;
        self.scopes.push(Vec::new());
        let result = self.block_inner();
        let scope = self.scopes.pop().unwrap();
        self.block_depth -= 1;
        let mut result = result?;
        // Owned values declared in this block are released at its end.
        // Releases are idempotent (slots are zeroed), so overlapping paths
        // stay safe.
        let owned: Vec<(usize, Type)> = scope.iter().filter(|(_, ty)| ty.owned()).copied().collect();
        if !owned.is_empty() { result.push(Statement::Frees(owned)); }
        // Moved markers for names that left scope are no longer meaningful,
        // and borrows recorded in this scope end with it.
        let live: std::collections::HashSet<usize> = self.names.values().map(|&(id, _, _)| id).collect();
        self.moved.retain(|id| live.contains(id));
        let depth_now = self.scopes.len();
        self.borrows.retain(|&(_, depth, _)| depth <= depth_now);
        self.call_borrows.retain(|&(_, depth)| depth <= depth_now);
        self.mut_borrows.retain(|&(_, depth)| depth <= depth_now);
        Ok(result)
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

    pub(super) fn adjudicate_deferred_conflicts(&mut self, function: &Function) -> Result<(), ae_span::Diagnostic> {
        let mut_sigs = borrowscope::mut_borrow_positions(self.functions, self.generics);
        let outcome = borrowscope::adjudicate_deferred(function, &self.deferred_write_conflicts, &mut_sigs);
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
        let outcome = borrowscope::adjudicate_borrow_spawns(function, &self.deferred_borrow_spawns);
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
        self.scopes.iter().flatten().filter(|(_, ty)| ty.owned()).map(|&(id, ty)| (id, ty)).collect()
    }
    // A loop body runs again, so a value moved out of a variable declared
    // outside the loop must be re-initialized before the body ends; otherwise
    // the next iteration would read a moved-from handle. Without a fixed point
    // over the body (ADR-20) the move is rejected instead of guessed at.
    pub(super) fn close_loop_moves(&mut self, base: usize, before: std::collections::HashSet<usize>, offset: usize) -> Result<(), ae_span::Diagnostic> {
        let after = std::mem::replace(&mut self.moved, before.clone());
        let outer: Vec<usize> = self.scopes[..base.min(self.scopes.len())].iter().flatten().map(|&(id, _)| id).collect();
        for id in &after {
            // Values already moved before the loop are not this body's doing.
            if before.contains(id) || !outer.contains(id) { continue; }
            let name = self.names.iter().find(|(_, (vid, _, _))| vid == id).map(|(n, _)| n.clone()).unwrap_or_else(|| "?".to_owned());
            return Err(self.error_at(offset, &format!("cannot move '{name}' inside a loop body: the next iteration would read it after the move (re-initialize it before the body ends)")));
        }
        // Moves of loop-local values die with the body; nothing to merge.
        Ok(())
    }
    // break/continue leave the loop body: only owned values declared inside it die.
    pub(super) fn loop_local_owned(&self) -> Vec<(usize, Type)> {
        let base = *self.loop_scopes.last().unwrap_or(&0);
        self.scopes[base.min(self.scopes.len())..].iter().flatten().filter(|(_, ty)| ty.owned()).map(|&(id, ty)| (id, ty)).collect()
    }
    pub(super) fn expression_index_for(&self, file: &str, tokens: &Rc<Vec<Token>>) -> Rc<OnceCell<ResolvedExpressions>> {
        if self.file == file && Rc::ptr_eq(&self.tokens, tokens) {
            Rc::clone(&self.resolved_expressions)
        } else {
            Rc::default()
        }
    }

    fn resolved_expressions(&self) -> &ResolvedExpressions {
        self.resolved_expressions.get_or_init(|| {
            let mut expressions = ResolvedExpressions::default();
            let Ok(module) = ae_resolve::module::parse_module_tokens(&self.file, self.source, &self.tokens) else {
                return expressions;
            };
            let resolution = ae_resolve::resolve::resolve_with_tokens(&module, &[], &self.tokens);
            for occurrence in resolution.functions.iter().flat_map(|function| &function.occurrences) {
                let Some(definition) = occurrence.target.and_then(|id| resolution.defs.get(id.0 as usize)) else {
                    if occurrence.role == ae_resolve::Role::Call {
                        expressions.unresolved_calls.insert(occurrence.range.start);
                    }
                    continue;
                };
                match occurrence.role {
                    ae_resolve::Role::Read | ae_resolve::Role::Borrow if definition.kind.is_local() => {
                        expressions.local_bindings.insert(occurrence.range.start, definition.range.clone());
                        if occurrence.role == ae_resolve::Role::Read {
                            expressions.scalar_reads.insert(occurrence.range.start, definition.id);
                        }
                    }
                    ae_resolve::Role::Write if definition.kind.is_local() => {
                        expressions.scalar_writes.insert(occurrence.range.start, definition.id);
                    }
                    ae_resolve::Role::Call if definition.kind == ae_resolve::DefKind::Function
                        && definition.owner.is_none() && definition.name == occurrence.name =>
                    {
                        expressions.direct_calls.insert(occurrence.range.start);
                    }
                    ae_resolve::Role::Construct if definition.kind == ae_resolve::DefKind::Struct
                        && definition.name == occurrence.name =>
                    {
                        expressions.struct_constructors.insert(occurrence.range.start);
                    }
                    _ if definition.kind == ae_resolve::DefKind::Variant => {
                        expressions.enum_variants.insert(occurrence.range.start);
                    }
                    _ => {}
                }
            }
            expressions.specialization_arguments.extend(resolution.functions.iter().flat_map(|function| {
                function.specialization_arguments.iter().map(|(&offset, arguments)| (offset, arguments.clone()))
            }));
            expressions.pointer_cast_targets.extend(resolution.functions.iter().flat_map(|function| {
                function.pointer_cast_targets.iter().map(|(&offset, &mutable)| (offset, mutable))
            }));
            expressions.closure_headers.extend(resolution.functions.iter().flat_map(|function| {
                function.closure_headers.iter().map(|(&offset, header)| (offset, header.clone()))
            }));
            expressions.scalar_definitions.extend(resolution.defs.iter()
                .filter(|definition| definition.kind.is_local())
                .map(|definition| (definition.range.start, definition.id)));
            // Struct parameters (file-local, non-generic) are field-read
            // receivers for the body route: the resolver's inferred spelling
            // names the struct item, and the compiler's registry supplies
            // field types for the read checks. Marked before the statement
            // walk, whose field-read checks consult this set.
            for definition in resolution.defs.iter().filter(|entry| entry.kind == ae_resolve::DefKind::Parameter) {
                let names_struct = definition_struct_item(definition)
                    .and_then(|item| resolution.defs.get(item.0 as usize))
                    .is_some_and(|item| {
                        item.kind == ae_resolve::DefKind::Struct && self.struct_names.contains_key(&item.name)
                    });
                if names_struct {
                    expressions.struct_definitions.insert(definition.id);
                }
            }
            let function_statements = module.functions.iter()
                .filter_map(|function| function.body.as_ref()).flatten();
            let closure_statements = resolution.functions.iter()
                .flat_map(|function| function.closure_bodies.iter())
                .flat_map(|body| body.iter());
            for statement in function_statements.chain(closure_statements) {
                ae_syntax::statement::visit_statements(statement, &mut |statement| {
                    use ae_syntax::statement::StatementKind;
                    if let StatementKind::Binding { name_range, initializer, .. } = &statement.kind {
                        if matches!(initializer.kind, ae_syntax::expression::ExpressionKind::Array(_)) {
                            if let Some(def) = expressions.scalar_definitions.get(&name_range.start) {
                                expressions.array_definitions.insert(*def);
                            }
                        }
                    }
                    let kind = match &statement.kind {
                        StatementKind::Binding { name_range, mutable, .. } => Some(IndexedStatement::Binding {
                            name_start: name_range.start,
                            name_end: name_range.end,
                            mutable: *mutable,
                        }),
                        StatementKind::ForRange { binding_range, .. } => Some(IndexedStatement::For {
                            binding_start: binding_range.start,
                            binding_end: binding_range.end,
                            is_range: true,
                        }),
                        StatementKind::ForIn { binding_range, .. } => Some(IndexedStatement::For {
                            binding_start: binding_range.start,
                            binding_end: binding_range.end,
                            is_range: false,
                        }),
                        StatementKind::Print(argument) => Some(IndexedStatement::Print(argument.range.start)),
                        StatementKind::Assert(argument) => Some(IndexedStatement::Assert(argument.range.start)),
                        StatementKind::Break => Some(IndexedStatement::Break),
                        StatementKind::Continue => Some(IndexedStatement::Continue),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        expressions.statements.insert(statement.range.start, (kind, statement.range.end));
                    }
                    if let ae_syntax::statement::StatementKind::PointerStore { pointer, pointer_range, .. } = &statement.kind {
                        expressions.pointer_store_names.insert(pointer_range.start, pointer.clone());
                    }
                    if let ae_syntax::statement::StatementKind::Match { arms, .. } = &statement.kind {
                        for arm in arms {
                            expressions.match_patterns.insert(arm.pattern.range.start, arm.pattern.clone());
                        }
                    }
                    for top_level in statement.expressions() {
                        expressions.values.insert(top_level.range.start, Rc::new(top_level.clone()));
                        visit_expression_tree(top_level, &mut |expression| {
                            // A field read on a struct parameter joins the
                            // body route: the owner struct comes from the
                            // parameter's spelling or a nested marked field.
                            // Scalar fields are value-eligible; struct-typed
                            // fields are chain-only receivers (binding one as
                            // a value would move out of the borrowed struct).
                            if let ae_syntax::expression::ExpressionKind::Field { base, name } = &expression.kind {
                                if let Some(sid) = field_owner_struct(base, &expressions.scalar_reads, &expressions.struct_definitions, &resolution, self.struct_names, self.structs) {
                                    if let Some(field_ty) = field_ty_in(sid, name, self.structs) {
                                        if scalar_type(field_ty) {
                                            expressions.struct_field_reads.insert(expression.range.start);
                                        } else if matches!(field_ty, Type::Struct(_) | Type::StructRef(_) | Type::StructMut(_)) {
                                            expressions.struct_field_chains.insert(expression.range.start);
                                        }
                                    }
                                }
                            }
                        });
                    }
                    if let ae_syntax::statement::StatementKind::Assign { target, .. } = &statement.kind {
                        if let ae_syntax::expression::ExpressionKind::Index { index, .. } = &target.kind {
                            // The typed store checks the base and operator, then lowers the index.
                            expressions.values.insert(index.range.start, Rc::new(index.as_ref().clone()));
                        }
                    }
                });
            }
            // Array parameters are arrays for the body route too: an array
            // type spelling starts with `[` (after the borrow amp), where
            // vec/map/composite spellings start with an identifier. The
            // resolver keeps the pointee opaque (`& unknown`), so the spelled
            // tokens are the arbiter here.
            for function in &module.functions {
                let ae_syntax::declaration::DeclarationKind::Function { parameters, .. } = &function.declaration.kind else {
                    continue;
                };
                for parameter in parameters {
                    if parameter.ty.start == parameter.ty.end {
                        continue;
                    }
                    let mut index = self.tokens.partition_point(|token| token.offset < parameter.ty.start);
                    if self.tokens.get(index).map(|token| token.kind.clone()) == Some(Kind::Amp) {
                        index += 1;
                    }
                    if self.tokens.get(index).map(|token| token.kind.clone()) == Some(Kind::LBracket) {
                        if let Some(def) = expressions.scalar_definitions.get(&parameter.name_range.start) {
                            expressions.array_definitions.insert(*def);
                        }
                    }
                }
            }
            for function in module.functions {
                if function.impl_owner.is_some() || !function.declaration.type_parameters.is_empty() {
                    continue;
                }
                let (Some(range), Some(body)) = (function.declaration.body, function.body) else {
                    continue;
                };
                let eligible = body.iter().all(|statement| scalar_body_statement(statement, &expressions, &|_, _| true));
                if eligible {
                    expressions.scalar_bodies.insert(range.start, body.into());
                }
            }
            expressions
        })
    }

    pub(super) fn shared_specialization_arguments(&self, offset: usize) -> Option<std::sync::Arc<[std::ops::Range<usize>]>> {
        self.resolved_expressions().specialization_arguments.get(&offset).cloned()
    }

    pub(super) fn shared_pointer_cast_qualifier(&self) -> Option<bool> {
        let offset = self.tokens.get(self.pos + 2)?.offset;
        self.resolved_expressions().pointer_cast_targets.get(&offset).copied()
    }

    pub(super) fn shared_closure_header(&self) -> Option<std::sync::Arc<ae_syntax::expression::Expression>> {
        let offset = self.tokens.get(self.pos)?.offset;
        self.resolved_expressions().closure_headers.get(&offset).cloned()
    }

    pub(super) fn lower_resolved_scalar_body(&mut self) -> Option<Result<Vec<Statement>, ae_span::Diagnostic>> {
        // Array parameters are borrowed views (`&[i64; N]` is the only array
        // parameter form, and `Type::Array` bakes the borrow in): their
        // element reads and stores flow through the same arms as local
        // arrays. File-local struct parameters join as immutable borrowed
        // field-read receivers only — by-value and `&mut` structs carry
        // ownership effects (moves, drop at scope exit) that stay typed.
        // Borrows of aggregates themselves (`&a`, `&p`) stay typed too — a
        // borrow occurrence never lands in `scalar_reads` — and so do calls
        // to aggregate-taking functions, which `supports_call` excludes.
        if !self.subst.is_empty() || self.receiver.is_some()
            || !scalar_type(self.return_type)
            || !self.names.values().all(|&(_, _, ty)| {
                scalar_type(ty) || matches!(ty, Type::Array(_) | Type::StructRef(_))
            })
        {
            return None;
        }
        let resolved = self.resolved_expressions();
        let body = resolved.scalar_bodies.get(&self.tokens[self.pos].offset)?.clone();
        let supports_call = |name: &str, offset: usize| {
            !RESERVED.contains(&name) && self.functions.get(name).is_some_and(|signature| {
                // File-local resolution leaves imports open. The compiler's
                // registry already contains the signatures of loaded modules.
                let visible = if signature.file == self.file {
                    resolved.direct_calls.contains(&offset)
                } else {
                    signature.file != PRELUDE && signature.public && resolved.unresolved_calls.contains(&offset)
                };
                visible && !signature.extern_c && !is_unsafe_fn(signature.id)
                    && scalar_type(signature.ret) && signature.params.iter().copied().all(scalar_type)
            })
        };
        if !body.iter().all(|statement| scalar_body_statement(statement, resolved, &supports_call)) {
            return None;
        }
        let mut slots = HashMap::new();
        for &(slot, _, ty) in self.names.values() {
            let range = self.binding_ranges.get(&slot)?;
            slots.insert(*resolved.scalar_definitions.get(&range.start)?, (slot, ty));
        }
        Some(self.lower_scalar_statements(&body, &mut slots))
    }

    fn lower_scalar_statements(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        use ae_syntax::statement::StatementKind;
        self.take(Kind::LBrace)?;
        let mut statements = Vec::with_capacity(body.len());
        for statement in body {
            if matches!(statement.kind, StatementKind::ForRange { .. }) {
                self.lower_scalar_range(statement, slots, &mut statements)?;
            } else {
                statements.push(self.lower_scalar_statement(statement, slots)?);
            }
        }
        self.take(Kind::RBrace)?;
        for statement in body {
            if let StatementKind::Binding { name, name_range, .. } = &statement.kind {
                self.names.remove(name);
                slots.remove(&self.resolved_expressions().scalar_definitions[&name_range.start]);
            }
        }
        Ok(statements)
    }

    fn lower_scalar_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::statement::{ElseBranch, StatementKind};
        let lowered = match &statement.kind {
            StatementKind::Binding { name, name_range, mutable, initializer } => {
                self.seek_scalar_offset(name_range.end);
                if RESERVED.contains(&name.as_str()) {
                    return Err(self.fail(&format!("'{name}' is a built-in name and cannot be a variable")));
                }
                if self.struct_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names a struct and cannot be a variable")));
                }
                if self.enum_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names an enum and cannot be a variable")));
                }
                if self.names.contains_key(name) {
                    return Err(self.duplicate_binding_error(name));
                }
                let slot = self.next_var;
                self.next_var += 1;
                let (statement, ty) = if let ae_syntax::expression::ExpressionKind::Array(elements) = &initializer.kind {
                    if elements.is_empty() {
                        self.seek_scalar_offset(initializer.range.start);
                        self.pos += 1;
                        return Err(self.fail("array must contain at least one element"));
                    }
                    let mut values = Vec::with_capacity(elements.len());
                    for (index, element) in elements.iter().enumerate() {
                        if index == 4096 {
                            self.seek_scalar_offset(element.range.start);
                            return Err(self.fail("array element limit exceeded (4096)"));
                        }
                        let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                        if ty != Type::Int {
                            self.seek_scalar_offset(element.range.end);
                            return Err(self.fail("array elements must be i64"));
                        }
                        values.push(value);
                    }
                    (Statement::Array(slot, values), Type::Array(elements.len()))
                } else {
                    let (value, ty) = self.lower_scalar_body_expression(initializer, slots)?;
                    (Statement::Declare(slot, value), ty)
                };
                let def = self.resolved_expressions().scalar_definitions[&name_range.start];
                slots.insert(def, (slot, ty));
                self.names.insert(name.clone(), (slot, *mutable, ty));
                self.binding_ranges.insert(slot, name_range.clone());
                statement
            }
            StatementKind::Assign { target, operator, value } => {
                if let ae_syntax::expression::ExpressionKind::Index { base, index } = &target.kind {
                    // An array element store (`a[i] = v`): eligibility
                    // confirmed a resolved array binding and the plain
                    // operator. The checks mirror the typed store: the
                    // binding must be mutable (E0103), the index and the
                    // element must be i64 (E0201 for the index).
                    let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                        unreachable!("scalar store eligibility");
                    };
                    let def = self.resolved_expressions().scalar_writes[&base.range.start];
                    let (slot, Type::Array(len)) = slots[&def] else {
                        unreachable!("resolved array store");
                    };
                    if !self.names[name].1 {
                        self.seek_scalar_offset(base.range.start);
                        let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                        return Err(self.immutable_binding_error(diagnostic, slot));
                    }
                    self.seek_scalar_offset(base.range.end);
                    let (line, column) = self.position();
                    let (element_index, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                    if index_ty != Type::Int {
                        self.seek_scalar_offset(index.range.end);
                        return Err(self.fail("array index must be an integer").with_code("E0201"));
                    }
                    self.seek_scalar_offset(value.range.start);
                    let (element, element_ty) = self.lower_scalar_body_expression(value, slots)?;
                    if element_ty != Type::Int {
                        self.seek_scalar_offset(value.range.end);
                        return Err(self.fail("array elements must be i64"));
                    }
                    self.seek_scalar_offset(statement.range.end);
                    return Ok(Statement::Store(slot, len, element_index, element, line, column));
                }
                let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &target.kind else {
                    unreachable!("scalar assignment eligibility");
                };
                let def = self.resolved_expressions().scalar_writes[&target.range.start];
                let (slot, ty) = slots[&def];
                if !self.names[name].1 {
                    self.seek_scalar_offset(target.range.start);
                    let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                    return Err(self.immutable_binding_error(diagnostic, slot));
                }
                self.seek_scalar_offset(target.range.end);
                let compound = compound_assignment_operator(operator);
                if let Some(op) = &compound {
                    if ty != Type::Int && !(ty == Type::F64 && *op != Kind::Percent) {
                        return Err(self.fail(&format!("compound assignment is not supported for type '{}'", ty.name())));
                    }
                }
                self.seek_scalar_offset(value.range.start);
                let (line, column) = self.position();
                let (rhs, rhs_ty) = self.lower_scalar_body_expression(value, slots)?;
                self.seek_scalar_offset(statement.range.end);
                if rhs_ty != ty {
                    return Err(self.fail(&format!("cannot assign a {} value to {} variable '{name}'", rhs_ty.name(), ty.name())));
                }
                let value = if let Some(op) = compound {
                    let lhs = Expr::Variable(slot, ty, line, column);
                    Expr::Binary(Box::new(lhs), op, Box::new(rhs), line, column, ty)
                } else {
                    rhs
                };
                Statement::Assign(slot, value, false)
            }
            StatementKind::Print(argument) => {
                let (value, _) = self.lower_scalar_body_expression(argument, slots)?;
                Statement::Print(Printed::Expr(value))
            }
            StatementKind::Assert(argument) => {
                self.seek_scalar_offset(statement.range.start);
                let (line, column) = self.position();
                let (condition, ty) = self.lower_scalar_body_expression(argument, slots)?;
                if ty != Type::Bool {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail("assert requires a boolean condition"));
                }
                Statement::Assert(condition, line, column)
            }
            StatementKind::Return(value) => {
                if self.in_main {
                    self.seek_scalar_offset(statement.range.start);
                    return Err(self.fail("return is not supported in main"));
                }
                let (value, ty) = self.lower_scalar_body_expression(value, slots)?;
                self.seek_scalar_offset(statement.range.end);
                if ty != self.return_type {
                    return Err(self.fail(&format!("return type must be {}", self.return_type.name())).with_code("E0204"));
                }
                Statement::Return(value, Vec::new())
            }
            StatementKind::If { condition, then_body, else_body } => {
                let value = self.lower_scalar_condition(condition, slots)?;
                let yes = self.lower_scalar_statements(then_body, slots)?;
                let no = if let Some(branch) = else_body {
                    self.pos += 1;
                    match branch {
                        ElseBranch::Block(body) => self.lower_scalar_statements(body, slots)?,
                        ElseBranch::If(statement) => vec![self.lower_scalar_statement(statement, slots)?],
                    }
                } else {
                    Vec::new()
                };
                Statement::If(value, yes, no)
            }
            StatementKind::While { condition, body } => {
                let condition = self.lower_scalar_condition(condition, slots)?;
                if self.loop_depth >= 64 {
                    return Err(self.fail("loop nesting limit exceeded (64)"));
                }
                Statement::While(condition, self.lower_scalar_loop_body(body, slots, None)?)
            }
            StatementKind::Break | StatementKind::Continue => {
                let is_break = matches!(statement.kind, StatementKind::Break);
                if self.loop_depth == 0 {
                    self.seek_scalar_offset(statement.range.start);
                    let keyword = if is_break { "break" } else { "continue" };
                    return Err(self.fail(&format!("'{keyword}' outside of a loop")));
                }
                if is_break {
                    Statement::Break
                } else {
                    Statement::Continue(self.loop_labels.last().copied().flatten())
                }
            }
            _ => unreachable!("scalar body eligibility"),
        };
        self.seek_scalar_offset(statement.range.end);
        Ok(lowered)
    }

    fn lower_scalar_range(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        statements: &mut Vec<Statement>,
    ) -> Result<(), ae_span::Diagnostic> {
        let ae_syntax::statement::StatementKind::ForRange { binding, binding_range, start, end, body } = &statement.kind else {
            unreachable!("scalar range eligibility");
        };
        self.seek_scalar_offset(binding_range.end);
        if RESERVED.contains(&binding.as_str()) {
            return Err(self.fail(&format!("'{binding}' is a built-in name and cannot be a loop variable")));
        }
        if self.names.contains_key(binding) {
            return Err(self.duplicate_binding_error(binding));
        }
        let start = self.lower_scalar_range_bound(start, slots)?;
        let end = self.lower_scalar_range_bound(end, slots)?;
        if self.loop_depth >= 64 {
            return Err(self.fail("loop nesting limit exceeded (64)"));
        }
        let (line, column) = self.position();
        let end_slot = self.next_var;
        let slot = end_slot + 1;
        self.next_var += 2;
        let label = self.next_label;
        self.next_label += 1;
        let def = self.resolved_expressions().scalar_definitions[&binding_range.start];
        slots.insert(def, (slot, Type::Int));
        self.names.insert(binding.clone(), (slot, false, Type::Int));
        let body = self.lower_scalar_loop_body(body, slots, Some(label));
        slots.remove(&def);
        self.names.remove(binding);
        let body = body?;
        statements.push(Statement::Declare(slot, start));
        statements.push(Statement::Declare(end_slot, end));
        statements.push(Statement::For(slot, end_slot, body, label, line, column));
        Ok(())
    }

    fn lower_scalar_loop_body(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        label: Option<usize>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        self.loop_depth += 1;
        self.loop_labels.push(label);
        let result = self.lower_scalar_statements(body, slots);
        self.loop_labels.pop();
        self.loop_depth -= 1;
        result
    }

    fn lower_scalar_range_bound(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(expression, slots)?;
        self.seek_scalar_offset(expression.range.end);
        if ty != Type::Int {
            return Err(self.fail("range bounds must be i64"));
        }
        Ok(value)
    }

    fn lower_scalar_condition(
        &mut self,
        condition: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(condition, slots)?;
        self.seek_scalar_offset(condition.range.end);
        if ty != Type::Bool {
            return Err(self.fail("condition must be a boolean expression").with_code("E0205"));
        }
        Ok(value)
    }

    fn seek_scalar_offset(&mut self, offset: usize) {
        self.pos = self.tokens.partition_point(|token| token.offset < offset);
    }

    fn field_ty_checked(&self, sid: usize, name: &str) -> Result<Type, ae_span::Diagnostic> {
        let defn = &self.structs[sid];
        defn.fields.iter().position(|(n, _)| n == name)
            .map(|fidx| defn.fields[fidx].1)
            .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", defn.name, name)))
    }

    fn lower_scalar_body_expression(
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
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let def = self.resolved_expressions().scalar_reads[&expression.range.start];
                let &(slot, ty) = slots.get(&def).ok_or_else(|| {
                    self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101")
                })?;
                self.seek_scalar_offset(expression.range.start);
                let (line, column) = self.position();
                (Expr::Variable(slot, ty, line, column), ty)
            }
            ExpressionKind::Group(inner) => self.lower_scalar_body_expression(inner, slots)?,
            ExpressionKind::Field { base, name } => {
                if matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))) {
                    // A struct parameter's scalar field read: the slot carries
                    // the struct type with its registry id, so the field type
                    // comes straight from the compiler's own table.
                    let def = self.resolved_expressions().scalar_reads[&base.range.start];
                    let (slot, struct_ty) = slots[&def];
                    let sid = match struct_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        _ => unreachable!("resolved struct binding"),
                    };
                    self.seek_scalar_offset(base.range.start);
                    let (line, column) = self.position();
                    let fty = self.field_ty_checked(sid, name)?;
                    (Expr::Field(Box::new(Expr::Variable(slot, struct_ty, line, column)), name.clone(), fty), fty)
                } else {
                    // A nested read (`o.inner.x`): the marked base lowers
                    // recursively and its declared type supplies the owner.
                    let (base_expr, base_ty) = self.lower_scalar_body_expression(base, slots)?;
                    let sid = match base_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        _ => unreachable!("resolved struct field base"),
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
                self.lower_prefix(operator.clone(), operand, position)?
            }
            ExpressionKind::Binary { left, operator, right } => {
                let lhs = self.lower_scalar_body_expression(left, slots)?;
                self.seek_scalar_offset(left.range.end);
                let position = self.position();
                let rhs = self.lower_scalar_body_expression(right, slots)?;
                self.seek_scalar_offset(expression.range.end);
                self.lower_infix(lhs, operator.clone(), rhs, position)?
            }
            ExpressionKind::Call { callee, arguments } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else {
                    unreachable!("scalar call eligibility");
                };
                let signature = self.functions.get(name).expect("scalar call signature").clone();
                let mut args = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    self.seek_scalar_offset(argument.range.start);
                    if index >= 64 {
                        return Err(self.fail("function argument limit exceeded (64)"));
                    }
                    let expected = signature.params.get(index)
                        .ok_or_else(|| self.fail(&format!("too many arguments to '{name}'")))?;
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    if actual != *expected {
                        self.seek_scalar_offset(argument.range.end);
                        return Err(self.fail(&format!("argument {} to '{name}' must be {}", index + 1, expected.name())));
                    }
                    args.push(Argument::Value(value));
                }
                self.seek_scalar_offset(expression.range.end);
                if args.len() != signature.params.len() {
                    return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", signature.params.len(), args.len())));
                }
                (Expr::Call(signature.id, args, signature.ret), signature.ret)
            }
            ExpressionKind::Index { base, index } => {
                let def = self.resolved_expressions().scalar_reads[&base.range.start];
                let (slot, Type::Array(len)) = slots[&def] else {
                    unreachable!("resolved array binding");
                };
                self.seek_scalar_offset(base.range.end);
                let (line, column) = self.position();
                let (value, ty) = self.lower_scalar_body_expression(index, slots)?;
                if ty != Type::Int {
                    self.seek_scalar_offset(index.range.end);
                    return Err(self.fail("array index must be an integer").with_code("E0201"));
                }
                (Expr::Index(slot, len, Box::new(value), line, column), Type::Int)
            }
            _ => unreachable!("scalar expression eligibility"),
        })
    }

    fn lower_resolved_local(&mut self, name: &str, offset: usize) -> Option<(Expr, Type)> {
        let (id, _, ty) = *self.names.get(name)?;
        if self.moved.contains(&id) || self.shadow_ok || !self.subst.is_empty() {
            return None;
        }
        let definition = self.resolved_expressions().local_bindings.get(&offset)?;
        // Match the resolver's source definition to the active compiler slot.
        if self.binding_ranges.get(&id)? != definition {
            return None;
        }
        let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
        Some((Expr::Variable(id, ty, token.line, token.column), ty))
    }

    fn lower_shared_argument(
        &mut self,
        argument: &ae_syntax::expression::Expression,
        expected: Type,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if expected.owned() || expected == Type::StrView {
            if let ExpressionKind::Atom(Kind::Ident(name)) = &argument.kind {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(name, argument.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                if expected == ty && ty.owned() {
                    if self.variable_move_needs_deferral(slot)?
                        || effects.call_borrows.contains(&slot)
                        || effects.mut_borrows.contains(&slot)
                    {
                        effects.conflicts.push((slot, argument.range.start, name.clone(), "move", None));
                    }
                    effects.moves.push(slot);
                }
                return Some((value, ty));
            }
        }
        if expected == Type::StrView {
            self.lower_shared_read(argument, remaining, nodes_left, effects)
        } else {
            self.lower_shared_expression(argument, remaining, nodes_left, effects)
        }
    }

    fn lower_shared_field(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Field { base, name } = &expression.kind else {
            return None;
        };
        let remaining = remaining.checked_sub(1)?;
        let (value, ty) = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(binding)) => {
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(binding, base.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                (value, ty)
            }
            ExpressionKind::Field { .. } => self.lower_shared_field(base, remaining, nodes_left, effects)?,
            // The typed parser requires parentheses around a literal receiver.
            ExpressionKind::Struct { .. } => return None,
            _ => self.lower_shared_expression(base, remaining, nodes_left, effects)?,
        };
        let sid = match ty {
            Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
            _ => return None,
        };
        let (_, ty) = self.structs.get(sid)?.fields.iter().find(|(field, _)| field == name)?;
        Some((Expr::Field(Box::new(value), name.clone(), *ty), *ty))
    }

    fn lower_shared_read(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            return matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::String | Type::StrView)
                .then_some((value, ty));
        }
        self.lower_shared_expression(expression, remaining, nodes_left, effects)
    }

    fn lower_shared_variant(
        &mut self,
        target: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Qualified { base, name: variant } = &target.kind else {
            return None;
        };
        let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
            return None;
        };
        let member = self.tokens.partition_point(|token| token.offset < target.range.end).checked_sub(1)?;
        let offset = self.tokens[member].offset;
        if !self.resolved_expressions().enum_variants.contains(&offset) {
            return None;
        }
        let &eid = self.enum_names.get(name)?;
        let def = self.enums.get(eid)?;
        self.require_public("enum", def.name, &def.file, def.public).ok()?;
        let (variant_id, (_, payload)) = def.variants.iter().enumerate().find(|(_, (name, _))| name == variant)?;
        if arguments.len() != payload.len() {
            return None;
        }
        let (enum_name, payload) = (def.name, payload.clone());
        let mut values = Vec::with_capacity(payload.len());
        for (argument, expected) in arguments.iter().zip(payload) {
            let (value, actual) = if expected == Type::String {
                self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?
            } else {
                self.lower_shared_expression(argument, remaining, nodes_left, effects)?
            };
            if expected != actual {
                return None;
            }
            values.push(value);
        }
        Some((Expr::EnumLit(enum_name, eid, variant_id, values), Type::Enum(enum_name, eid)))
    }

    fn lower_shared_expression(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            // Owned fields need the typed parser's context-sensitive move checks.
            return matches!(ty, Type::Int | Type::F64 | Type::Bool).then_some((value, ty));
        }
        let remaining = remaining.checked_sub(1)?;
        // The typed Pratt parser charges operands, groups and prefixes.
        // Infix and postfix cast nodes add no charge beyond their operands.
        if !matches!(expression.kind, ExpressionKind::Binary { .. } | ExpressionKind::PointerCast { .. }) {
            *nodes_left = nodes_left.checked_sub(1)?;
        }
        match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => Some((Expr::Number(*value), Type::Int)),
            ExpressionKind::Atom(Kind::Float(value)) => Some((Expr::FloatLit(*value), Type::F64)),
            ExpressionKind::Atom(Kind::True) => Some((Expr::BoolLit(true), Type::Bool)),
            ExpressionKind::Atom(Kind::False) => Some((Expr::BoolLit(false), Type::Bool)),
            ExpressionKind::Atom(Kind::Text(value)) => {
                let offset = expression.range.start;
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
                Some((Expr::StrLit(value.clone(), token.line, token.column), Type::String))
            }
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let (_, _, ty) = self.names.get(name)?;
                if !matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::Closure(_) | Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                self.lower_resolved_local(name, expression.range.start)
            }
            ExpressionKind::Index { base, index } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return None;
                };
                let (Expr::Variable(slot, _, _, _), ty) = self.lower_resolved_local(name, base.range.start)? else {
                    return None;
                };
                if effects.moves.contains(&slot) {
                    return None;
                }
                let bracket = self.tokens.get(self.tokens.partition_point(|token| token.offset < base.range.end))?;
                if bracket.kind != Kind::LBracket {
                    return None;
                }
                let (line, column) = (bracket.line, bracket.column);
                let (index, Type::Int) = self.lower_shared_expression(index, remaining, nodes_left, effects)? else {
                    return None;
                };
                let index = Box::new(index);
                match ty {
                    Type::Array(len) => Some((Expr::Index(slot, len, index, line, column), Type::Int)),
                    Type::Vec | Type::VecRef | Type::VecF | Type::VecFRef => {
                        let borrowed = matches!(ty, Type::VecRef | Type::VecFRef);
                        let element = if matches!(ty, Type::VecF | Type::VecFRef) { Type::F64 } else { Type::Int };
                        Some((Expr::VecIndex(slot, index, line, column, borrowed, element), element))
                    }
                    _ => None,
                }
            }
            ExpressionKind::Struct { target, fields } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &target.kind else {
                    return None;
                };
                if std::env::var_os("AE_DEBUG_MIR").is_some()
                    || !self.resolved_expressions().struct_constructors.contains(&target.range.start)
                {
                    return None;
                }
                let &sid = self.struct_names.get(name)?;
                let def = self.structs.get(sid)?;
                self.require_public("struct", &def.name, &def.file, def.public).ok()?;
                if fields.len() != def.fields.len() {
                    return None;
                }
                let declared_fields = def.fields.clone();
                let mut values: Vec<_> = (0..fields.len()).map(|_| None).collect();
                for field in fields {
                    let slot = declared_fields.iter().position(|(name, _)| name == &field.name)?;
                    if values[slot].is_some() {
                        return None;
                    }
                    let expected = declared_fields[slot].1;
                    let (value, actual) = self.lower_shared_argument(&field.value, expected, remaining, nodes_left, effects)?;
                    if actual != expected {
                        return None;
                    }
                    values[slot] = Some(value);
                }
                Some((Expr::StructLit(sid, values.into_iter().collect::<Option<_>>()?), Type::Struct(sid)))
            }
            ExpressionKind::Vector(elements) => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let (line, column) = (token.line, token.column);
                let mut vector_type = None;
                let mut values = Vec::new();
                for element in elements {
                    let (value, element_type) = self.lower_shared_expression(element, remaining, nodes_left, effects)?;
                    let ty = match element_type {
                        Type::Int => Type::Vec,
                        Type::F64 => Type::VecF,
                        _ => return None,
                    };
                    if vector_type.is_some_and(|previous| previous != ty) {
                        return None;
                    }
                    vector_type = Some(ty);
                    values.push(value);
                }
                let ty = vector_type.unwrap_or(Type::Vec);
                Some((Expr::VecLit(values, ty, line, column), ty))
            }
            ExpressionKind::AddressCast { binding, binding_range, mutable, pointee } => {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (operand, ty) = self.lower_resolved_local(binding, binding_range.start)?;
                let name = self.source.get(pointee.clone())?;
                if super::types::scalar_type(name)? != ty || (*mutable && !self.names.get(binding)?.1) {
                    return None;
                }
                let start = self.tokens.partition_point(|token| token.offset < expression.range.start);
                let token = self.tokens.get(start)?;
                let id = super::intern_pointee(ty);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::BorrowAs(Box::new(operand), *mutable, token.line, token.column), ty))
            }
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                let (operand, base_type) = self.lower_shared_expression(base, remaining, nodes_left, effects)?;
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                let name = self.source.get(pointee.clone())?;
                let pointee = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end)?;
                let id = super::intern_pointee(pointee);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty))
            }
            ExpressionKind::Group(inner) => self.lower_shared_expression(inner, remaining, nodes_left, effects),
            ExpressionKind::Qualified { .. } => {
                self.lower_shared_variant(expression, &[], remaining, nodes_left, effects)
            }
            ExpressionKind::Call { callee, arguments } => {
                if matches!(callee.kind, ExpressionKind::Qualified { .. }) {
                    if let Some(last) = arguments.last() {
                        let next = self.tokens.partition_point(|token| token.offset < last.range.end);
                        if self.tokens.get(next)?.kind != Kind::RParen {
                            return None;
                        }
                    }
                    return self.lower_shared_variant(callee, arguments, remaining, nodes_left, effects);
                }
                let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else {
                    return None;
                };
                if self.names.contains_key(name) || RESERVED.contains(&name.as_str())
                    || !self.resolved_expressions().direct_calls.contains(&callee.range.start)
                {
                    return None;
                }
                let signature = self.functions.get(name)?;
                if signature.file != self.file || signature.extern_c || is_unsafe_fn(signature.id)
                    || arguments.len() != signature.params.len() || arguments.len() > 64
                    || !signature.params.iter().all(|ty| {
                        matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView
                            | Type::Vec | Type::VecF | Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::Struct(_) | Type::Enum(..) | Type::StructRef(_) | Type::StructMut(_))
                    })
                    || !matches!(signature.ret, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::VecF | Type::Struct(_) | Type::Enum(..))
                {
                    return None;
                }
                let (id, params, ret) = (signature.id, signature.params.clone(), signature.ret);
                let mut args = Vec::with_capacity(arguments.len());
                for (argument, expected) in arguments.iter().zip(params) {
                    if matches!(expected, Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_)) {
                        let ExpressionKind::BorrowArgument { name, name_range, mutable } = &argument.kind else {
                            return None;
                        };
                        if *mutable != matches!(expected, Type::StructMut(_)) {
                            return None;
                        }
                        let (Expr::Variable(slot, ..), actual) = self.lower_resolved_local(name, name_range.start)? else {
                            return None;
                        };
                        if effects.moves.contains(&slot) {
                            return None;
                        }
                        let address = match (expected, actual) {
                            (expected, actual) if expected == actual && !*mutable => false,
                            (Type::StructRef(wanted), Type::Struct(actual_id) | Type::StructMut(actual_id))
                                if wanted == actual_id =>
                            {
                                if effects.mut_borrows.contains(&slot)
                                    || self.mut_borrows.iter().any(|&(borrowed, _)| borrowed == slot)
                                {
                                    return None;
                                }
                                if !self.is_borrowed(slot) && !effects.call_borrows.contains(&slot) {
                                    effects.call_borrows.push(slot);
                                }
                                matches!(actual, Type::Struct(_))
                            }
                            (Type::StructMut(wanted), Type::Struct(actual_id) | Type::StructMut(actual_id))
                                if wanted == actual_id =>
                            {
                                if !self.names.get(name)?.1 || effects.call_borrows.contains(&slot)
                                    || effects.mut_borrows.contains(&slot)
                                {
                                    return None;
                                }
                                if self.is_borrowed(slot) {
                                    if !self.only_view_borrows_active(slot) {
                                        return None;
                                    }
                                    effects.conflicts.push((slot, name_range.start, name.clone(), "mutate", None));
                                }
                                effects.mut_borrows.push(slot);
                                matches!(actual, Type::Struct(_))
                            }
                            (Type::VecRef, Type::Vec) | (Type::VecFRef, Type::VecF)
                            | (Type::StrVecRef, Type::StrVec) => true,
                            _ => return None,
                        };
                        args.push(Argument::Borrow(slot, address));
                        continue;
                    }
                    let value = self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?;
                    args.push(match (expected, value) {
                        (Type::StrView, (value, Type::String | Type::StrView)) => Argument::View(value),
                        (expected, (value, actual)) if expected == actual => Argument::Value(value),
                        _ => return None,
                    });
                }
                Some((Expr::Call(id, args, ret), ret))
            }
            ExpressionKind::Prefix { operator, operand } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let position = (token.line, token.column);
                let operand = self.lower_shared_expression(operand, remaining, nodes_left, effects)?;
                self.lower_prefix(operator.clone(), operand, position).ok()
            }
            ExpressionKind::Binary { left, operator, right } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < left.range.end))?;
                if &token.kind != operator {
                    return None;
                }
                let position = (token.line, token.column);
                let lower_operand = if matches!(operator, Kind::Plus | Kind::EqEq | Kind::NotEq) {
                    Self::lower_shared_read
                } else {
                    Self::lower_shared_expression
                };
                let left = lower_operand(self, left, remaining, nodes_left, effects)?;
                let right = lower_operand(self, right, remaining, nodes_left, effects)?;
                self.lower_infix(left, operator.clone(), right, position).ok()
            }
            _ => None,
        }
    }

    fn commit_shared_effects(&mut self, effects: SharedExpressionEffects) {
        self.call_borrows.extend(effects.call_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.mut_borrows.extend(effects.mut_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.deferred_write_conflicts.extend(effects.conflicts);
        self.moved.extend(effects.moves);
    }

    fn shared_expression_start(kind: &Kind) -> bool {
        matches!(kind, Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False |
            Kind::Text(_) | Kind::LParen | Kind::Ident(_) | Kind::Amp)
            || ae_syntax::operator::prefix_binding_power(kind).is_some()
    }

    pub(super) fn lower_shared_value(&mut self, boundary: Option<Kind>, moving: bool) -> Option<(Expr, Type)> {
        if !Self::shared_expression_start(self.current()) {
            return None;
        }
        let offset = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&offset).cloned()?;
        let expression = match (boundary.as_ref(), &root.kind) {
            (Some(Kind::Question), ae_syntax::expression::ExpressionKind::Propagate(operand)) => operand.as_ref(),
            (Some(Kind::Question), _) => return None,
            _ => root.as_ref(),
        };
        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
        let next = &self.tokens.get(end)?.kind;
        // Leave the delimiter for the surrounding typed statement.
        // A propagation request lowers only the operand before '?'.
        let complete = match boundary.as_ref() {
            Some(expected) => next == expected,
            None => matches!(next, Kind::Semi | Kind::Comma | Kind::RParen | Kind::RBracket | Kind::DotDot),
        };
        if !complete {
            return None;
        }
        let (depth, budget) = if boundary.is_none() {
            (128usize.checked_sub(self.expression_depth)?.min(64),
             128usize.checked_sub(self.expression_nodes)?)
        } else {
            (64, 128)
        };
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let value = match &expression.kind {
            ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                if self.names.contains_key(name) =>
            {
                nodes_left = nodes_left.checked_sub(1)?;
                let value = self.lower_resolved_local(name, expression.range.start)?;
                let Expr::Variable(id, ty, ..) = &value.0 else { unreachable!("resolved local") };
                // Owned locals move on `moving`; copy-semantics handles
                // (Exec, Pool, Chan, Atomic, Mutex, Guard, TaskScope) and
                // views never move, so their reads route the same way the
                // typed parse reads them.
                if moving && ty.owned() {
                    let id = *id;
                    let start = self.pos;
                    self.pos = end;
                    if self.check_variable_move(id).is_err() {
                        self.pos = start;
                        return None;
                    }
                    self.moved.insert(id);
                }
                value
            }
            _ => self.lower_shared_expression(expression, depth, &mut nodes_left, &mut effects)?,
        };
        self.commit_shared_effects(effects);
        if boundary.is_none() {
            self.expression_nodes += budget - nodes_left;
        }
        self.pos = end;
        Some(value)
    }

    fn lower_shared_array(&mut self) -> Option<Vec<Expr>> {
        use ae_syntax::expression::ExpressionKind;

        let start = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&start).cloned()?;
        let ExpressionKind::Array(elements) = &root.kind else {
            return None;
        };
        let end = self.tokens.partition_point(|token| token.offset < root.range.end);
        if elements.is_empty() || elements.len() > 4096 || self.tokens.get(end)?.kind != Kind::Semi {
            return None;
        }
        let mut effects = SharedExpressionEffects::default();
        let mut values = Vec::with_capacity(elements.len());
        for element in elements {
            let mut nodes_left = 128;
            let (value, Type::Int) = self.lower_shared_expression(element, 64, &mut nodes_left, &mut effects)? else {
                return None;
            };
            values.push(value);
        }
        self.pos = end + 1;
        self.commit_shared_effects(effects);
        Some(values)
    }

    fn lower_shared_iterable(&mut self) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let offset = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&offset).cloned()?;
        let ExpressionKind::Atom(Kind::Ident(name)) = &root.kind else {
            return None;
        };
        let end = self.tokens.partition_point(|token| token.offset < root.range.end);
        if self.tokens.get(end)?.kind != Kind::LBrace || name == "Self" || self.struct_names.contains_key(name) {
            return None;
        }
        let value = self.lower_resolved_local(name, offset)?;
        if !matches!(value.1, Type::Tokens | Type::Vec | Type::VecRef | Type::VecF | Type::VecFRef | Type::StrVec | Type::StrVecRef) {
            return None;
        }
        self.pos = end;
        Some(value)
    }

    fn condition(&mut self) -> Result<Expr, ae_span::Diagnostic> {
        let (condition, ty) = match self.lower_shared_value(Some(Kind::LBrace), false) {
            Some(value) => value,
            None => self.expression(0, false)?,
        };
        if ty != Type::Bool {
            return Err(self.fail("condition must be a boolean expression").with_code("E0205"));
        }
        Ok(condition)
    }

    fn store_field_name(&mut self, target_offset: usize) -> Result<String, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;

        self.take(Kind::Dot)?;
        if let Some(root) = self.resolved_expressions().values.get(&target_offset) {
            let field = match &root.kind {
                ExpressionKind::Index { base, .. } => base.as_ref(),
                _ => root.as_ref(),
            };
            if let ExpressionKind::Field { base, name } = &field.kind {
                if matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && matches!(self.current(), Kind::Ident(token_name) if token_name == name)
                {
                    let name = name.clone();
                    self.pos += 1;
                    return Ok(name);
                }
            }
        }
        self.identifier()
    }

    fn indexed_statement(&self) -> Option<(IndexedStatement, usize)> {
        let start = self.tokens.get(self.pos)?.offset;
        let &(kind, end) = self.resolved_expressions().statements.get(&start)?;
        Some((kind, self.tokens.partition_point(|token| token.offset < end)))
    }

    /// The loop binding's name from the shared for statement, when the
    /// resolver tree indexed this `for` (ADR-0229 slice 5): the binding
    /// name and whether the iterable is a range. Falls back to the token
    /// reads when the index misses.
    fn for_header(&self, loop_offset: usize) -> Option<(String, bool)> {
        let Some((IndexedStatement::For { binding_start, binding_end, is_range }, _)) =
            self.resolved_expressions().statements.get(&loop_offset) else {
            return None;
        };
        let (binding_start, binding_end, is_range) = (*binding_start, *binding_end, *is_range);
        let name = self.source.get(binding_start..binding_end)?.to_owned();
        Some((name, is_range))
    }

    fn binding_header(&self) -> Result<ae_syntax::statement::BindingHeader<'_>, ae_span::Diagnostic> {
        let offset = self.tokens[self.pos].offset;
        if let Some(&(IndexedStatement::Binding { name_start, name_end, mutable }, _)) =
            self.resolved_expressions().statements.get(&offset)
        {
            let name_range = name_start..name_end;
            if let Some(name) = self.source.get(name_range.clone()) {
                return Ok(ae_syntax::statement::BindingHeader { name, name_range, mutable });
            }
        }
        ae_syntax::statement::parse_binding_header(&self.file, self.source, &self.tokens, self.pos)
    }

    fn lower_output_from_syntax(&mut self) -> Option<Statement> {
        use ae_syntax::expression::ExpressionKind;

        let [_, open, argument, ..] = &self.tokens[self.pos..] else {
            return None;
        };
        if open.kind != Kind::LParen || !Self::shared_expression_start(&argument.kind) {
            return None;
        }
        let (print, argument, end) = if let Some((kind, end)) = self.indexed_statement() {
            let (print, offset) = match kind {
                IndexedStatement::Print(offset) => (true, offset),
                IndexedStatement::Assert(offset) => (false, offset),
                _ => return None,
            };
            let argument = self.resolved_expressions().values.get(&offset).cloned()?;
            (print, argument, end)
        } else {
            use ae_syntax::statement::{parse_statement_tokens, StatementKind};

            let (statement, end) = parse_statement_tokens(&self.file, self.source, &self.tokens, self.pos).ok()?;
            let (print, argument) = match statement.kind {
                StatementKind::Print(value) => (true, value),
                StatementKind::Assert(value) => (false, value),
                _ => return None,
            };
            (print, Rc::new(argument), end)
        };

        // Commit argument effects and advance the cursor only after the entire
        // statement lowers successfully.
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        let lowered = if print {
            match &argument.kind {
                ExpressionKind::Atom(Kind::Text(value)) => Statement::Print(Printed::Text(value.clone())),
                _ => {
                    let (value, ty) = self.lower_shared_read(&argument, 64, &mut nodes_left, &mut effects)?;
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                        return None;
                    }
                    Statement::Print(Printed::Expr(value))
                }
            }
        } else {
            let (condition, Type::Bool) = self.lower_shared_expression(&argument, 64, &mut nodes_left, &mut effects)? else {
                return None;
            };
            let (line, column) = self.position();
            Statement::Assert(condition, line, column)
        };
        self.pos = end;
        self.commit_shared_effects(effects);
        Some(lowered)
    }

    pub(super) fn block_inner(&mut self) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        self.take(Kind::LBrace)?;
        let mut declared = Vec::new();
        let mut result = Vec::new();
        while *self.current() != Kind::RBrace {
            // A call argument's borrow covered only the statement that made
            // the call; the next statement may write the struct again.
            self.call_borrows.clear();
            self.mut_borrows.clear();
            match self.current().clone() {
                Kind::Let | Kind::Var => {
                    let header = self.binding_header()?;
                    let name = header.name.to_owned();
                    let binding_range = header.name_range;
                    let mutable = header.mutable;
                    self.pos += 2;
                    if RESERVED.contains(&name.as_str()) { return Err(self.fail(&format!("'{name}' is a built-in name and cannot be a variable"))); }
                    if self.struct_names.contains_key(&name) { return Err(self.fail(&format!("'{name}' names a struct and cannot be a variable"))); }
                    if self.enum_names.contains_key(&name) { return Err(self.fail(&format!("'{name}' names an enum and cannot be a variable"))); }
                    if self.names.contains_key(&name) { return Err(self.duplicate_binding_error(&name)); }
                    self.take(Kind::Equal)?;
                    let id = self.next_var; self.next_var += 1;
                    let shared_array = if *self.current() == Kind::LBracket { self.lower_shared_array() } else { None };
                    let (stmt, ty) = if let Some(values) = shared_array {
                        let len = values.len();
                        (Statement::Array(id, values), Type::Array(len))
                    } else if *self.current() == Kind::LBracket {
                        self.pos += 1;
                        if *self.current() == Kind::RBracket { return Err(self.fail("array must contain at least one element")); }
                        let values = ae_syntax::parse_comma_list(
                            self,
                            |parser, count| {
                                if count == 4096 {
                                    return Err(parser.fail("array element limit exceeded (4096)"));
                                }
                                let (element, element_ty) = parser.expression(0, false)?;
                                if element_ty != Type::Int {
                                    return Err(parser.fail("array elements must be i64"));
                                }
                                Ok(element)
                            },
                            |parser| {
                                if *parser.current() != Kind::Comma {
                                    return false;
                                }
                                parser.pos += 1;
                                true
                            },
                        )?;
                        self.take(Kind::RBracket)?; self.take(Kind::Semi)?;
                        let len = values.len(); (Statement::Array(id, values), Type::Array(len))
                    } else {
                        // The typed initializer consumes string values.
                        let (value, ty) = match self.lower_shared_value(Some(Kind::Question), true) {
                            Some(value) => value,
                            None => self.expression(0, true)?,
                        };
                        // `let x = call()?;` binds the Ok payload and returns the
                        // error to the caller instead of continuing.
                        if *self.current() == Kind::Question {
                            let (statement, bound_ty) = self.propagate_statement(value, ty, Some(id))?;
                            self.pos += 1;
                            self.take(Kind::Semi)?;
                            let bound_ty = bound_ty.expect("a binding propagation always produces a payload type");
                            self.names.insert(name.clone(), (id, mutable, bound_ty));
                            self.binding_ranges.insert(id, binding_range);
                            self.scopes.last_mut().unwrap().push((id, bound_ty));
                            declared.push(name);
                            result.push(statement);
                            continue;
                        }
                        // A view binds once (M2.5): the NLL regions treat view
                        // slots as declare-only — a mutable binding would let a
                        // reassignment re-point the view at different bytes
                        // with no kill in the region math, so it is rejected
                        // before it can exist.
                        if mutable && matches!(ty, Type::StrView | Type::Tokens) {
                            return Err(self.fail("a view binding cannot be mutable; declare it with 'let'"));
                        }
                        self.take(Kind::Semi)?;
                        // A borrow has no storage of its own: it lives only as a
                        // parameter or a pattern binding, never as a variable.
                        if matches!(ty, Type::VecRef | Type::StrVecRef) {
                            return Err(self.fail("cannot store a borrowed vector in a variable; pass it directly to a parameter that borrows"));
                        }
                        if matches!(ty, Type::StructRef(_) | Type::StructMut(_)) {
                            return Err(self.fail("cannot store a borrowed struct in a variable; pass it directly to a parameter that borrows"));
                        }
                        (Statement::Declare(id, value), ty)
                    };
                    self.names.insert(name.clone(), (id, mutable, ty));
                    self.binding_ranges.insert(id, binding_range);
                    self.scopes.last_mut().unwrap().push((id, ty));
                    declared.push(name);
                    if let Statement::Declare(_, init) = &stmt { self.record_borrow_bases(init); }
                    result.push(stmt);
                }
                Kind::Ident(name) => {
                    // Kept for diagnostics that must point at the statement's
                    // own name rather than at whatever token follows it.
                    let name_offset = self.tokens[self.pos].offset;
                    // `exit(code);` terminates the process. It releases the
                    // live owned values first, so the allocation account stays
                    // balanced and buffered output still flushes.
                    if name == "exit" && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        self.pos += 2;
                        let (code, code_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if code_ty != Type::Int { return Err(self.fail("exit expects an i64 status code")); }
                        self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                        result.push(Statement::Exit(code, self.live_owned()));
                        continue;
                    }
                    // Built-in mutation statements come before assignment so
                    // they stay unambiguous: push(v, x); pop(v);
                    // atomic_store(handle, value); — the seq_cst release
                    // half of the handoff (spec/concurrency.md §8).
                    if name == "cell_store" && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        self.pos += 2;
                        let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if handle_ty != Type::Atomic { return Err(self.fail("cell_store expects an atomic handle")); }
                        self.take(Kind::Comma)?;
                        let (value, value_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if value_ty != Type::Int { return Err(self.fail("cell_store expects an i64 value")); }
                        self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                        result.push(Statement::AtomicStore(Box::new(handle), Box::new(value)));
                        continue;
                    }
                    if matches!(name.as_str(), "shared_set" | "chan_close" | "task_spawn" | "task_yield" | "scope_spawn" | "pool_spawn") && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        result.push(self.concurrency_stmt(&name)?);
                        continue;
                    }
                    if matches!(name.as_str(), "push" | "pop" | "sort" | "reverse") && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        let (line, col) = self.position();
                        self.pos += 2;
                        let target = self.identifier()?;
                        let (id, mutable, ty) = *self.names.get(&target).ok_or_else(|| self.fail(&format!("unknown variable '{target}'")).with_code("E0101"))?;
                        if self.moved.contains(&id) { return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot use '{target}' because it was moved")).with_code("E0102"), id))); }
                        if !matches!(ty, Type::Vec | Type::StrVec | Type::VecF) { return Err(self.fail(&format!("{name} expects a vec, found {}", ty.name()))); }
                        if !mutable { return Err(self.immutable_binding_error(self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), id)); }
                        if self.is_borrowed(id) { return Err(self.fail(&format!("cannot modify '{target}' while it is borrowed"))); }
                        // Ordering in place: qsort for i64, byte order for
                        // strings, so sorted output is identical on both
                        // platforms. The vector keeps its length.
                        if name == "sort" || name == "reverse" {
                            if ty == Type::VecF { return Err(self.fail(&format!("{name} is not supported for vec[f64]; NaN has no total order"))); }
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            let strings = ty == Type::StrVec;
                            result.push(if name == "sort" { Statement::Sort(id, strings, line, col) } else { Statement::Reverse(id, strings, line, col) });
                            continue;
                        }
                        if name == "push" {
                            self.take(Kind::Comma)?;
                            let (value, value_ty) = match self.lower_shared_value(Some(Kind::RParen), ty == Type::StrVec) {
                                Some(pair) => pair,
                                None => self.expression(0, ty == Type::StrVec)?,
                            };
                            // A borrow is fine: the element is a copy, so a view
                            // can be stored without outliving its source.
                            let ok = match ty {
                                Type::Vec => value_ty == Type::Int,
                                Type::VecF => value_ty == Type::F64,
                                _ => matches!(value_ty, Type::String | Type::StrView),
                            };
                            if !ok { return Err(self.fail(&format!("vector elements must be {}", match ty { Type::Vec => "i64", Type::VecF => "f64", _ => "string" }))); }
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            if ty == Type::StrVec {
                                result.push(Statement::StrVecPush(id, value, line, col, value_ty == Type::StrView));
                            } else if ty == Type::VecF {
                                result.push(Statement::VecFPush(id, value, line, col));
                            } else {
                                result.push(Statement::VecPush(id, value, line, col));
                            }
                        } else {
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            if ty == Type::StrVec {
                                result.push(Statement::StrVecPop(id, line, col));
                            } else if ty == Type::VecF {
                                result.push(Statement::VecFPop(id, line, col));
                            } else {
                                result.push(Statement::VecPop(id, line, col));
                            }
                        }
                        continue;
                    }
                    if !self.names.contains_key(&name) {
                        // A call used only for its effect. The value must be
                        // propagated with '?', so a failure is never dropped.
                        let (value, ty) = match self.lower_shared_value(Some(Kind::Question), true) {
                            Some(value) => value,
                            None => self.expression(0, true)?,
                        };
                        if *self.current() != Kind::Question {
                            return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
                        }
                        let (statement, _) = self.propagate_statement(value, ty, None)?;
                        self.pos += 1;
                        self.take(Kind::Semi)?;
                        result.push(statement);
                        continue;
                    }
                    let (id, mutable, ty) = *self.names.get(&name).ok_or_else(|| self.fail(&format!("unknown variable '{name}'")).with_code("E0101"))?;
                    if !mutable { return Err(self.immutable_binding_error(self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103"), id)); }
                    self.pos += 1;
                    if (ty == Type::Map || ty == Type::MapF) && *self.current() == Kind::LBracket {
                        // `m[key] = value;` inserts or overwrites. The key is
                        // copied by the runtime, so a view or temporary is fine.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position();
                        self.take(Kind::LBracket)?;
                        let (key, key_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail("map keys must be strings")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != map_element_ty(map_kind_of(ty)) { return Err(self.fail(&format!("map values must be {}", map_element_ty(map_kind_of(ty)).name()))); }
                        result.push(Statement::MapPut(MapPlace::Var(id, map_kind_of(ty)), key, value, line, col));
                    } else if let Type::Array(len) = ty {
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if index_ty != Type::Int { return Err(self.fail("array index must be an integer").with_code("E0201")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != Type::Int { return Err(self.fail("array elements must be i64")); }
                        result.push(Statement::Store(id, len, index, value, line, col));
                    } else if ty == Type::StrVec && *self.current() == Kind::LBracket {
                        // Overwriting an element releases the old string, so a
                        // live borrow of any element blocks it.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if !matches!(value_ty, Type::String | Type::StrView) { return Err(self.fail("vector elements must be string")); }
                        result.push(Statement::StrVecStore(id, index, value, line, col, value_ty == Type::StrView));
                    } else if (ty == Type::Vec || ty == Type::VecF) && *self.current() == Kind::LBracket {
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if ty == Type::VecF {
                            if value_ty != Type::F64 { return Err(self.fail("vector elements must be f64")); }
                            result.push(Statement::VecFStore(id, index, value, line, col));
                        } else {
                            if value_ty != Type::Int { return Err(self.fail("vector elements must be i64")); }
                            result.push(Statement::VecStore(id, index, value, line, col));
                        }
                    } else if let Type::Enum(_, _) = ty {
                        // Assigning an enum releases the old payload first.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        self.take(Kind::Equal)?;
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != ty { return Err(self.fail(&format!("cannot assign a {} value to {} variable", value_ty.name(), ty.name()))); }
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        result.push(Statement::Assign(id, value, true));
                    } else if let Type::StructRef(_) = ty {
                        // A struct borrow is read-only (ADR-0184): neither the
                        // pointer nor anything behind it may be written, so
                        // both spellings get their own diagnostic instead of
                        // falling into the scalar-assignment path.
                        if *self.current() == Kind::Dot {
                            return Err(self.fail(&format!("cannot write through '{name}': struct borrows are read-only in this version")));
                        }
                        return Err(self.fail(&format!("cannot assign to '{name}': struct borrows are read-only in this version")));
                    } else if let Type::StructMut(sid) = ty {
                        // A mutable borrow exists to write through (ADR-0185):
                        // field stores go through the pointer. Whole-struct
                        // assignment would have to release the old pointee
                        // before storing, which needs a dedicated statement —
                        // rejected for now rather than guessed at.
                        if *self.current() == Kind::Dot {
                            let fname = self.store_field_name(name_offset)?;
                            let fty = {
                                let def = self.structs[sid].clone();
                                let fidx = def.fields.iter().position(|(n, _)| *n == fname)
                                    .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", def.name, fname)))?;
                                def.fields[fidx].1
                            };
                            if matches!(fty, Type::Map | Type::MapF) {
                                return Err(self.fail("map fields cannot be written through a '&mut' borrow yet; copy the map out with a by-value receiver instead"));
                            }
                            self.take(Kind::Equal)?;
                            // An owned field write consumes a string/vec
                            // right side — the field owns the bytes after
                            // the write, so the source must forfeit them
                            // (the same rule the scalar assignment uses).
                            let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                            if value_ty != fty { return Err(self.fail(&format!("field '{}' must be {}", fname, fty.name()))); }
                            result.push(Statement::FieldStore(id, fname, fty, value, ty));
                            continue;
                        }
                        return Err(self.fail(&format!("cannot assign to '{name}' itself through a '&mut' borrow; write its fields instead")));
                    } else if let Type::Struct(sid) = ty {
                        if *self.current() == Kind::Dot {
                            let fname = self.store_field_name(name_offset)?;
                            let fty = {
                                let def = self.structs[sid].clone();
                                let fidx = def.fields.iter().position(|(n, _)| *n == fname)
                                    .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", def.name, fname)))?;
                                def.fields[fidx].1
                            };
                            if (fty == Type::Map || fty == Type::MapF) && *self.current() == Kind::LBracket {
                                // `s.counts[key] = n;` writes through the field,
                                // so the record itself stays in place.
                                if self.is_borrowed(id) {
                                    return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                                }
                                let (line, col) = self.position();
                                self.take(Kind::LBracket)?;
                                let (key, key_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                                    Some(pair) => pair,
                                    None => self.expression(0, false)?,
                                };
                                if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail("map keys must be strings")); }
                                self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                                let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                                if value_ty != map_element_ty(map_kind_of(fty)) { return Err(self.fail(&format!("map values must be {}", map_element_ty(map_kind_of(fty)).name()))); }
                                result.push(Statement::MapPut(MapPlace::Field(id, fname, map_kind_of(fty)), key, value, line, col));
                                continue;
                            }
                            // A view into one of the fields points at the bytes
                            // this write would release, so the write is only
                            // allowed once the view is gone (ADR-52) — the
                            // view-kind case is deferred to the graph's NLL
                            // adjudication at the variable granularity the
                            // borrow uses; container borrows reject on sight.
                            if self.is_borrowed(id) && !self.all_borrows_are_views(id) {
                                return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                            }
                            if self.is_borrowed(id) {
                                self.deferred_write_conflicts.push((id, name_offset, name.clone(), "modify", Some(fname.clone())));
                            }
                            let compound_op = match *self.current() {
                                Kind::PlusEqual => Some(Kind::Plus),
                                Kind::MinusEqual => Some(Kind::Minus),
                                Kind::StarEqual => Some(Kind::Star),
                                Kind::SlashEqual => Some(Kind::Slash),
                                Kind::PercentEqual => Some(Kind::Percent),
                                _ => None,
                            };
                            if let Some(op) = compound_op {
                                let allowed = matches!(fty, Type::Int)
                                    || (matches!(fty, Type::F64) && op != Kind::Percent)
                                    || (fty == Type::String && op == Kind::Plus);
                                if !allowed {
                                    return Err(self.fail(&format!("compound assignment is not supported for field type '{}'", fty.name())));
                                }
                                self.pos += 1;
                                let (line, col) = self.position();
let (rhs, rhs_ty) = match self.lower_shared_value(None, false) {
    Some(pair) => pair,
    None => self.expression(0, false)?,
};
                                self.take(Kind::Semi)?;
                                if rhs_ty != fty {
                                    return Err(self.fail(&format!("field '{}' must be {}", fname, fty.name())));
                                }
                                let base_var = Expr::Variable(id, ty, line, col);
                                let lhs_field = Expr::Field(Box::new(base_var), fname.clone(), fty);
                                let value = Expr::Binary(Box::new(lhs_field), op, Box::new(rhs), line, col, fty);
                                result.push(Statement::FieldStore(id, fname, fty, value, ty));
                                continue;
                            }
                            self.take(Kind::Equal)?;
                            // Same consuming rule as the `&mut` write: the
                            // field owns the bytes afterwards.
                            let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                            if value_ty != fty { return Err(self.fail(&format!("field '{}' must be {}", fname, fty.name()))); }
                            result.push(Statement::FieldStore(id, fname, fty, value, ty));
                            continue;
                        }
                        // Whole-struct assignment moves a fresh value in.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        self.take(Kind::Equal)?;
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != ty { return Err(self.fail(&format!("cannot assign a {} value to {} variable", value_ty.name(), ty.name()))); }
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        result.push(Statement::Assign(id, value, true));
                    } else {
                        // A string's borrows can be view-kind borrows, whose
                        // region dies after the view's last use (M2.5 L1):
                        // the conflict is deferred to the graph's
                        // adjudication instead of rejected on sight, so a
                        // write after the last view use parses. Container
                        // borrows keep the on-sight rejection.
                        if self.is_borrowed(id) && !self.all_borrows_are_views(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        if self.is_borrowed(id) {
                            self.deferred_write_conflicts.push((id, name_offset, name.clone(), "assign to", None));
                        }
                        let compound_op = compound_assignment_operator(self.current());
                        if let Some(op) = compound_op {
                            let allowed = matches!(ty, Type::Int)
                                || (matches!(ty, Type::F64) && op != Kind::Percent)
                                || (ty == Type::String && op == Kind::Plus);
                            if !allowed {
                                return Err(self.fail(&format!("compound assignment is not supported for type '{}'", ty.name())));
                            }
                            self.pos += 1;
                            let (line, col) = self.position();
                            let (rhs, rhs_ty) = match self.lower_shared_value(None, false) {
                                Some(pair) => pair,
                                None => self.expression(0, false)?,
                            };
                            self.take(Kind::Semi)?;
                            if rhs_ty != ty {
                                return Err(self.fail(&format!("cannot assign a {} value to {} variable '{name}'", rhs_ty.name(), ty.name())));
                            }
                            let lhs_var = Expr::Variable(id, ty, line, col);
                            let value = Expr::Binary(Box::new(lhs_var), op, Box::new(rhs), line, col, ty);
                            result.push(Statement::Assign(id, value, false));
                            continue;
                        }
                        self.take(Kind::Equal)?;
                        // A view slot never re-points (M2.5): the region math
                        // has no kills, so a reassigned view would both
                        // overstate the old region and silently borrow a base
                        // the recorded introduction never names. Bind a new
                        // view with 'let' instead.
                        if matches!(ty, Type::StrView | Type::Tokens) {
                            return Err(self.fail("a view binding cannot be reassigned; declare a new view with 'let'"));
                        }
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != ty { return Err(self.fail(&format!("cannot assign a {} value to {} variable '{name}'", value_ty.name(), ty.name()))); }
                        // The right-hand side may not consume the target itself;
                        // assigning to an already-moved variable is a legal
                        // re-initialization, so the move flag clears either way.
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        // Every owned slot releases its old value before the new
                        // one moves in; scalars have nothing to release.
                        result.push(Statement::Assign(id, value, ty.owned()));
                    }
                }
                Kind::Return => {
                    if self.in_main { return Err(self.fail("return is not supported in main")); }
                    self.pos += 1;
                    let (value, ty) = match self.lower_shared_value(None, true) {
                        Some(pair) => pair,
                        None => self.expression(0, true)?,
                    };
                    self.take(Kind::Semi)?;
                    if matches!(ty, Type::Closure(_)) { return Err(self.fail("a closure cannot be returned: it has no storage, and its captures live in this frame")); }
                    if ty != self.return_type { return Err(self.fail(&format!("return type must be {}", self.return_type.name())).with_code("E0204")); }
                    // Every live owned value that is not returned is released.
                    result.push(Statement::Return(value, self.live_owned()));
                }
                Kind::Star => {
                    // `*p = value;` (M2.12 slice 3): a write through a `*mut`
                    // pointer, inside `unsafe` only. The pointer is a local
                    // variable; the pointee's current storage is whatever the
                    // pointer names.
                    let (line, column) = self.position();
                    self.pos += 1;
                    let offset = self.tokens[self.pos].offset;
                    let name = match self.resolved_expressions().pointer_store_names.get(&offset).cloned() {
                        Some(name) => {
                            self.pos += 1;
                            name
                        }
                        None => self.identifier()?,
                    };
                    let Some((var_id, _, Type::PtrMut(pointee_id))) = self.names.get(&name).cloned() else {
                        return Err(self.fail(&format!("'*{name}' requires a *mut pointer variable")));
                    };
                    if self.unsafe_depth == 0 {
                        return Err(self.error_at(line, "writing through a raw pointer requires 'unsafe'").with_code("E0700"));
                    }
                    self.take(Kind::Equal)?;
                    let (value, value_ty) = match self.lower_shared_value(None, false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    let pointee = super::ptr_pointee(pointee_id);
                    if value_ty != pointee {
                        return Err(self.fail(&format!("cannot write a {} value through *mut {}", value_ty.name(), pointee.name())));
                    }
                    if pointee.owned() {
                        return Err(self.fail("writing an owned value through a pointer is not supported yet; the pointer would take over its release"));
                    }
                    self.take(Kind::Semi)?;
                    result.push(Statement::PtrStore(var_id, value, line, column));
                }
                Kind::Unsafe => {
                    // M2.12 slice 1 (spec/unsafe.md 3.1): an `unsafe { ... }`
                    // block at statement position. The block is a scoping
                    // container; the unchecked operations (raw deref, pointer
                    // store, extern/unsafe calls) gate on unsafe_depth. W0702
                    // (spec/unsafe.md U4): a block containing none of them is
                    // silent decoration, and says so on stderr without failing
                    // the compile.
                    self.pos += 1;
                    let block_line = self.tokens[self.pos].line;
                    let block_column = self.tokens[self.pos].column;
                    self.unsafe_depth += 1;
                    let inner = self.block()?;
                    self.unsafe_depth -= 1;
                    if !self.block_has_unchecked_operation(&inner) {
                        eprintln!("ae: {}:{}: warning: unnecessary unsafe block (W0702): the body contains no unchecked operation", block_line, block_column);
                    }
                    result.push(Statement::Unsafe(inner));
                }
                Kind::If => result.push(self.if_statement()?),
                Kind::Match => result.push(self.match_statement()?),
                Kind::While => {
                    let loop_offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let condition = self.condition()?;
                    if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                    self.loop_depth += 1;
                    let base = self.scopes.len();
                    let moved_before = self.moved.clone();
                    self.loop_scopes.push(base);
                    self.loop_labels.push(None);
                    let body = self.block();
                    self.loop_labels.pop();
                    self.loop_scopes.pop();
                    self.loop_depth -= 1;
                    let body = body?;
                    self.close_loop_moves(base, moved_before, loop_offset)?;
                    result.push(Statement::While(condition, body));
                }
                Kind::For => {
                    let loop_offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let shared_for = self.for_header(loop_offset);
                    let var_name = match &shared_for {
                        Some((name, _)) => {
                            self.pos += 1;
                            name.clone()
                        }
                        None => self.identifier()?,
                    };
                    if RESERVED.contains(&var_name.as_str()) { return Err(self.fail(&format!("'{var_name}' is a built-in name and cannot be a loop variable"))); }
                    if self.names.contains_key(&var_name) { return Err(self.duplicate_binding_error(&var_name)); }
                    self.take(Kind::In)?;
                    let (first, first_ty) = match self.lower_shared_iterable() {
                        Some(value) => value,
                        None => self.expression(0, false)?,
                    };
                    if first_ty == Type::Tokens {
                        // `for token in cursor` streams the tokens as read-only
                        // views: no vector, no per-token allocation, and the
                        // loop body sees one token at a time (ADR-53). Each
                        // loop restarts at the beginning of the base text.
                        let cursor = match &first {
                            Expr::Variable(id, Type::Tokens, _, _) => *id,
                            _ => return Err(self.fail("for-in over a token cursor needs a bare cursor variable: bind it first, e.g. let toks = tokens(text, \" \");")),
                        };
                        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                        let (line, col) = self.position();
                        let var = self.next_var; self.next_var += 1;
                        let label = self.next_label; self.next_label += 1;
                        self.loop_depth += 1;
                        let base = self.scopes.len();
                        let moved_before = self.moved.clone();
                        self.scopes.push(vec![(var, Type::StrView)]);
                        self.loop_scopes.push(self.scopes.len());
                        self.loop_labels.push(Some(label));
                        self.names.insert(var_name.clone(), (var, false, Type::StrView));
                        let body = self.block();
                        self.loop_labels.pop();
                        self.loop_scopes.pop();
                        self.scopes.pop().unwrap();
                        self.loop_depth -= 1;
                        self.names.remove(&var_name);
                        let body = body?;
                        self.close_loop_moves(base, moved_before, loop_offset)?;
                        result.push(Statement::ForTokens(cursor, var, body, label, line, col));
                        continue;
                    }
                    if *self.current() != Kind::DotDot {
                        // `for x in vec-variable` iterates owned elements by
                        // value: desugars to a hidden range loop plus an
                        // immutable element declaration per iteration.
                        // Borrowed string vectors iterate the same way, with
                        // element access going through the pointer.
                        let (v_id, elem_ty, by_ref) = match (&first, first_ty) {
                            (Expr::Variable(id, Type::Vec, _, _), Type::Vec) => (*id, Type::Int, false),
                            (Expr::Variable(id, Type::StrVec, _, _), Type::StrVec) => (*id, Type::StrView, false),
                            (Expr::Variable(id, Type::StrVecRef, _, _), Type::StrVecRef) => (*id, Type::StrView, true),
                            (Expr::Variable(id, Type::VecRef, _, _), Type::VecRef) => (*id, Type::Int, false),
                            (Expr::Variable(id, Type::VecF, _, _), Type::VecF) => (*id, Type::F64, false),
                            (Expr::Variable(id, Type::VecFRef, _, _), Type::VecFRef) => (*id, Type::F64, false),
                            _ => return Err(self.fail("for-in needs a bare vec variable or an int range (start..end)")),
                        };
                        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                        let (line, col) = self.position();
                        let end_var = self.next_var; self.next_var += 1;
                        let counter = self.next_var; self.next_var += 1;
                        let var = self.next_var; self.next_var += 1;
                        let label = self.next_label; self.next_label += 1;
                        self.loop_depth += 1;
                        let base = self.scopes.len();
                        let moved_before = self.moved.clone();
                        self.scopes.push(vec![(var, elem_ty), (counter, Type::Int), (end_var, Type::Int)]);
                        self.loop_scopes.push(self.scopes.len());
                        self.loop_labels.push(Some(label));
                        self.names.insert(var_name.clone(), (var, false, elem_ty));
                        // The container is borrowed for the whole loop.
                        let container_depth = self.scopes.len();
                        self.borrows.push((v_id, container_depth, BorrowKind::Container));
                        let body = self.block();
                        self.loop_labels.pop();
                        self.loop_scopes.pop();
                        self.scopes.pop().unwrap();
                        self.loop_depth -= 1;
                        self.names.remove(&var_name);
                        self.borrows.retain(|&(id, _, _)| id != v_id);
                        let mut body = body?;
                        if self.moved.contains(&v_id) { return Err(self.fail(&format!("cannot move the vector being iterated (a move of it happens in this loop)"))); }
                        self.close_loop_moves(base, moved_before, loop_offset)?;
                        // Per-iteration element binding, then the user's body.
                        let element = match elem_ty {
                            Type::StrView => Expr::StrVecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int, line, col)), line, col, by_ref),
                            _ => Expr::VecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int, line, col)), line, col, first_ty == Type::VecRef || first_ty == Type::VecFRef, elem_ty),
                        };
                        body.insert(0, Statement::Declare(var, element));
                        result.push(Statement::Declare(end_var, Expr::Len(Box::new(first), line, col)));
                        result.push(Statement::Declare(counter, Expr::Number(0)));
                        result.push(Statement::For(counter, end_var, body, label, line, col));
                        continue;
                    }
                    if first_ty != Type::Int { return Err(self.fail("range bounds must be i64")); }
                    self.take(Kind::DotDot)?;
                    let (end, end_ty) = match self.lower_shared_value(Some(Kind::LBrace), false) {
                        Some(value) => value,
                        None => self.expression(0, false)?,
                    };
                    if end_ty != Type::Int { return Err(self.fail("range bounds must be i64")); }
                    if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                    let (line, col) = self.position();
                    // Hidden storage: bounds evaluate once, in order.
                    let end_var = self.next_var; self.next_var += 1;
                    let var = self.next_var; self.next_var += 1;
                    let label = self.next_label; self.next_label += 1;
                    self.loop_depth += 1;
                    let base = self.scopes.len();
                    let moved_before = self.moved.clone();
                    self.scopes.push(vec![(var, Type::Int)]);
                    self.loop_scopes.push(self.scopes.len());
                    self.loop_labels.push(Some(label));
                    self.names.insert(var_name.clone(), (var, false, Type::Int));
                    let body = self.block();
                    self.loop_labels.pop();
                    self.loop_scopes.pop();
                    let for_scope = self.scopes.pop().unwrap();
                    self.loop_depth -= 1;
                    self.names.remove(&var_name);
                    let body = body?;
                    self.close_loop_moves(base, moved_before, loop_offset)?;
                    debug_assert!(for_scope.iter().all(|(_, ty)| *ty != Type::String));
                    // Bounds evaluate once, left to right, before any iteration.
                    result.push(Statement::Declare(var, first));
                    result.push(Statement::Declare(end_var, end));
                    result.push(Statement::For(var, end_var, body, label, line, col));
                }
                Kind::Break | Kind::Continue => {
                    let keyword = if *self.current() == Kind::Break { "break" } else { "continue" };
                    if self.loop_depth == 0 { return Err(self.fail(&format!("'{keyword}' outside of a loop"))); }
                    let (is_break, end) = match self.indexed_statement() {
                        Some((IndexedStatement::Break, end)) => (true, end),
                        Some((IndexedStatement::Continue, end)) => (false, end),
                        _ => {
                            let (statement, end) = ae_syntax::statement::parse_statement_tokens(
                                &self.file, self.source, &self.tokens, self.pos,
                            )?;
                            (matches!(statement.kind, ae_syntax::statement::StatementKind::Break), end)
                        }
                    };
                    self.pos = end;
                    let owned = self.loop_local_owned();
                    if !owned.is_empty() { result.push(Statement::Frees(owned)); }
                    result.push(if is_break {
                        Statement::Break
                    } else {
                        Statement::Continue(self.loop_labels.last().copied().flatten())
                    });
                }
                Kind::Assert => {
                    if let Some(statement) = self.lower_output_from_syntax() {
                        result.push(statement);
                        continue;
                    }
                    let (line, col) = self.position();
                    self.pos += 1; self.take(Kind::LParen)?;
                    let (condition, ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    if ty != Type::Bool { return Err(self.fail("assert requires a boolean condition")); }
                    self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                    result.push(Statement::Assert(condition, line, col));
                }
                Kind::Print => {
                    if let Some(statement) = self.lower_output_from_syntax() {
                        result.push(statement);
                        continue;
                    }
                    self.pos += 1; self.take(Kind::LParen)?;
                    // print borrows its value; only `+` consumes strings.
                    let (value, ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                        return Err(self.fail(&format!("cannot print a value of type {}", ty.name())).with_code("E0206"));
                    }
                    self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                    result.push(Statement::Print(Printed::Expr(value)));
                }
                _ => return Err(self.fail("expected declaration, assignment, print, or '}'")),
            };
        }
        self.take(Kind::RBrace)?;
        for name in declared { self.names.remove(&name); }
        Ok(result)
    }
    pub(super) fn if_statement(&mut self) -> Result<Statement, ae_span::Diagnostic> {
        self.pos += 1;
        let condition = self.condition()?;
        let snapshot = self.moved.clone();
        let yes = self.block()?;
        let after_yes = std::mem::replace(&mut self.moved, snapshot.clone());
        let no = if *self.current() == Kind::Else {
            self.pos += 1;
            // `else if` chains recurse instead of nesting braces.
            if *self.current() == Kind::If { vec![self.if_statement()?] } else { self.block()? }
        } else { Vec::new() };
        // A variable moved in either branch is treated as moved after the if.
        for id in after_yes { self.moved.insert(id); }
        Ok(Statement::If(condition, yes, no))
    }
    // `expr?` propagates the error variant of a result enum: on `Err` every
    // live owned value is released and the enum itself is returned, so a caller
    // never observes a half-finished result. On `Ok` the payload is bound (or
    // released when the statement only wanted the side effect).
    pub(super) fn propagate_statement(&mut self, value: Expr, ty: Type, binding: Option<usize>) -> Result<(Statement, Option<Type>), ae_span::Diagnostic> {
        let eid = match ty {
            Type::Enum(_, eid) => eid,
            _ => return Err(self.fail(&format!("'?' needs a result or option value, found {}", ty.name()))),
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
    // `match subject { Enum::Variant(a, b) => { ... } ... }` is a statement.
    // Coverage must be exhaustive and each variant may appear once, so there is
    // no fall-through path to reason about. The subject is borrowed for the
    // whole match: arms can read payloads but never move or overwrite it.
    //
    // The arm pattern itself comes from the shared syntax parser, so both
    // parsers agree on the grammar and its byte ranges; the checks below are
    // the semantic layer (declaration shape, binding kinds, exhaustiveness).
    pub(super) fn match_statement(&mut self) -> Result<Statement, ae_span::Diagnostic> {
        self.pos += 1;
        let bare_enum = match self.current() {
            Kind::Ident(name) => name != "Self" && !self.struct_names.contains_key(name)
                && matches!(self.names.get(name), Some((_, _, Type::Enum(..))))
                && self.tokens.get(self.pos + 1).is_some_and(|token| token.kind == Kind::LBrace),
            _ => false,
        };
        let shared = if bare_enum { self.lower_shared_value(Some(Kind::LBrace), false) } else { None };
        let (subject, subject_ty) = match shared {
            Some(value) => value,
            None => self.expression(0, false)?,
        };
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
            if enum_name != def.pattern {
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
            // The declaration decides the payload shape; `()` is a parse-level
            // empty list, so an empty or mismatched list lands here.
            let payloads = match payload {
                Some(_) if fields.is_empty() => {
                    return Err(self.error_at(variant_range.end,
                        &format!("variant '{}::{}' has no payload", def.name, vname)));
                }
                None if !fields.is_empty() => {
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
            let mut bound: Vec<String> = Vec::new();
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
                self.scopes.last_mut().unwrap().push((id, bty));
                bound.push(bname);
                bindings.push((id, field, index));
            }
            self.take(Kind::FatArrow)?;
            let body = self.block()?;
            self.scopes.pop().unwrap();
            for name in bound { self.names.remove(&name); }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_output_matches_resolver_definition_to_live_slot() {
        let source = "struct Point { x: i64, y: i64 } struct Inner { x: i64 } struct Outer { inner: Inner, tag: i64 } struct Holder { ready: bool, text: string } fn scalar() -> bool { let local = (2 + 3) * 4; return local == 20; } fn mismatch() -> i64 { return true; } fn branches() -> i64 { if false { let same = 1; return same; } else if true { let same = 2; return same; } else { let same = 3; return same; } } fn loops() -> i64 { while true { while false { continue; } break; } return 7; } fn bad_loop() -> i64 { continue; return 0; } fn ranges() -> i64 { for i in 2..5 { while false { continue; } if i == 3 { continue; } } let i = 7; return i; } fn bad_range() -> i64 { for j in false..5 {} return 0; } fn countdown() -> i64 { var n = 4; n = 2; while n > 0 { n -= 1; } return n; } fn frozen() -> i64 { let n = 1; n = 2; return n; } fn output() -> i64 { print(2); assert(2 == 2); return 0; } fn bad_assert() -> i64 { assert(5); return 0; } fn entry_shape() -> i64 { print(23); } fn calls() -> i64 { return compute(compute(2)); } fn wrong_call() -> i64 { return compute(false); } fn cross_module() -> i64 { return imported_scalar(2); } fn private_module() -> i64 { return imported_private(3); } fn local_array() -> i64 { let a = [4, 5]; return a[1]; } fn bad_array() -> i64 { let a = [4, 5]; return a[false]; } fn local_array_store() -> i64 { var a = [4, 5]; a[1] = 9; return a[1]; } fn bad_store() -> i64 { var a = [4, 5]; a[false] = 1; return a[0]; } fn frozen_store() -> i64 { let a = [4, 5]; a[0] = 9; return a[0]; } fn bad_store_value() -> i64 { var a = [4, 5]; a[0] = false; return a[0]; } fn compound_store() -> i64 { var a = [4, 5]; a[0] += 1; return a[0]; } fn array_param(a: &[i64; 2]) -> i64 { return a[1]; } fn array_param_store(a: &[i64; 2]) -> i64 { a[0] = 3; return 0; } fn field_read(p: &Point) -> i64 { return p.x + p.y; } fn field_text(h: &Holder) -> i64 { return h.text; } fn nested_read(o: &Outer) -> i64 { return o.inner.x; } fn compute(x: i64) -> i64 { return x; } fn consume(s: string) -> i64 { return 1; } fn main() { let text = \"hello\"; let flag = true; var value = flag; if flag {} print(flag); print(compute(2)); assert(consume(text)); print(consume(text)); let v = vec[1.0, 2.0]; print(vec[1]); print(v[0]); let raw = &value as *const bool; let view = raw as *mut f64; let write = &mut value as *mut bool; print(compute(choose[i64, vec[f64]](2, vec[1.0]))); let pair = Pair[i64, vec[f64]] { first: 1, second: vec[1.0] }; let made = Pair[i64, vec[f64]]::new(); let variant = Choice[i64, vec[f64]]::Some(1); let empty = Choice[i64, vec[f64]]::empty(); let compound = raw as *const shared[i64]; task_spawn(ex, || -> i64 { return 0; }); let closure = |x: i64| -> bool { return x == 0; }; }";
        let tokens = Rc::new(ae_syntax::lex_diagnostic(source).unwrap());
        let print_token = tokens.partition_point(|token| token.offset < source.find("print(flag)").unwrap());
        let call_token = tokens.iter().position(|token| token.offset == source.find("print(compute").unwrap()).unwrap();
        let condition_token = tokens.partition_point(|token| token.offset < source.find("if flag").unwrap() + 3);
        let initializer = source.find("flag;").unwrap();
        let initializer_token = tokens.iter().position(|token| token.offset == initializer).unwrap();
        let binding = source.find("flag").unwrap();
        let mut functions = SharedFunctions::default();
        functions.insert("compute".into(), Signature {
            id: 9, params: vec![Type::Int], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        functions.insert("imported_scalar".into(), Signature {
            id: 10, params: vec![Type::Int], ret: Type::Int, file: "imported.ae".into(),
            public: true, extern_c: false,
        });
        functions.insert("imported_private".into(), Signature {
            id: 11, params: vec![Type::Int], ret: Type::Int, file: "imported.ae".into(),
            public: false, extern_c: false,
        });
        let mut structs = Vec::new();
        structs.push(StructDef { name: "Point".into(), fields: vec![("x".into(), Type::Int), ("y".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None });
        structs.push(StructDef { name: "Inner".into(), fields: vec![("x".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None });
        structs.push(StructDef { name: "Outer".into(), fields: vec![("inner".into(), Type::Struct(1)), ("tag".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None });
        structs.push(StructDef { name: "Holder".into(), fields: vec![("ready".into(), Type::Bool), ("text".into(), Type::String)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None });
        let mut struct_names = HashMap::new();
        struct_names.insert("Point".into(), 0);
        struct_names.insert("Holder".into(), 3);
        struct_names.insert("Inner".into(), 1);
        struct_names.insert("Outer".into(), 2);
        let mut enums = Vec::new();
        let mut enum_names = HashMap::new();
        let mut generics = Generics::default();
        let mut parser = make_parser(
            "case.ae", source, tokens, &mut functions, &mut structs,
            &mut struct_names, &mut enums, &mut enum_names, &mut generics,
        );
        parser.return_type = Type::Bool;
        parser.seek_scalar_offset(source.find("{ let local").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved scalar route").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(0, _), Statement::Return(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), _)]));
        parser.return_type = Type::Int;
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let mismatch = parser.lower_resolved_scalar_body().expect("resolved error route").err().unwrap();
        assert_eq!(mismatch.message, "return type must be i64");

        parser.seek_scalar_offset(source.find("{ if false").unwrap());
        let branches = parser.lower_resolved_scalar_body().expect("resolved branches").unwrap();
        let [Statement::If(_, yes, otherwise)] = branches.as_slice() else { panic!("expected if") };
        let [Statement::If(_, other_yes, other_no)] = otherwise.as_slice() else { panic!("expected else if") };
        for (index, body) in [yes, other_yes, other_no].into_iter().enumerate() {
            assert!(matches!(body.as_slice(), [Statement::Declare(slot, _), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] if *slot == index + 1 && slot == read));
        }

        parser.seek_scalar_offset(source.find("{ while true").unwrap());
        let loops = parser.lower_resolved_scalar_body().expect("resolved loops").unwrap();
        let [Statement::While(_, outer), Statement::Return(Expr::Number(7), _)] = loops.as_slice() else {
            panic!("expected loop and return");
        };
        let [Statement::While(_, inner), Statement::Break] = outer.as_slice() else {
            panic!("expected nested loop and break");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        parser.seek_scalar_offset(source.find("{ continue; return").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved loop error").err().unwrap();
        assert_eq!(error.message, "'continue' outside of a loop");
        assert_eq!(parser.loop_depth, 0);

        parser.seek_scalar_offset(source.find("{ for i in 2").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved range").unwrap();
        let [Statement::Declare(start, Expr::Number(2)), Statement::Declare(end, Expr::Number(5)), Statement::For(counter, limit, loop_body, label, ..), Statement::Declare(reused, Expr::Number(7)), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] = body.as_slice() else {
            panic!("expected range bounds and loop");
        };
        assert_eq!((start, end), (counter, limit));
        assert_eq!(*start, *end + 1);
        assert_ne!(counter, reused);
        assert_eq!(reused, read);
        let [Statement::While(_, inner), Statement::If(_, branch, _)] = loop_body.as_slice() else {
            panic!("expected nested while and conditional");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        assert!(matches!(branch.as_slice(), [Statement::Continue(Some(jump))] if jump == label));

        parser.seek_scalar_offset(source.find("{ for j in false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved range error").err().unwrap();
        assert_eq!(error.message, "range bounds must be i64");

        parser.seek_scalar_offset(source.find("{ var n = 4").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved assignments").unwrap();
        let [Statement::Declare(slot, _), Statement::Assign(plain, Expr::Number(2), false), Statement::While(_, loop_body), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] = body.as_slice() else {
            panic!("expected scalar assignments");
        };
        assert_eq!(slot, plain);
        assert_eq!(slot, read);
        assert!(matches!(loop_body.as_slice(), [Statement::Assign(compound, Expr::Binary(_, Kind::Minus, _, _, _, Type::Int), false)] if compound == slot));

        parser.seek_scalar_offset(source.find("{ let n = 1").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable assignment").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'n'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("n");

        parser.seek_scalar_offset(source.find("{ print(2);").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved output").unwrap();
        let [Statement::Print(Printed::Expr(Expr::Number(2))), Statement::Assert(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), 1, column), Statement::Return(Expr::Number(0), _)] = body.as_slice() else {
            panic!("expected print, assert, and return");
        };
        assert_eq!(*column, source.find("assert(2 == 2)").unwrap() + 1);
        parser.seek_scalar_offset(source.find("{ assert(5)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved assert error").err().unwrap();
        assert_eq!(error.message, "assert requires a boolean condition");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.in_main = true;
        parser.seek_scalar_offset(source.find("{ print(23)").unwrap());
        let entry = parser.lower_resolved_scalar_body().expect("resolved entry body").unwrap();
        assert!(matches!(entry.as_slice(), [Statement::Print(Printed::Expr(Expr::Number(23)))]));
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved entry return error").err().unwrap();
        assert_eq!(error.message, "return is not supported in main");
        assert_eq!(*parser.current(), Kind::Return);
        parser.in_main = false;

        parser.seek_scalar_offset(source.find("{ return compute(compute").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved direct calls").unwrap();
        let [Statement::Return(Expr::Call(9, args, Type::Int), _)] = body.as_slice() else {
            panic!("expected direct call");
        };
        assert!(matches!(args.as_slice(), [Argument::Value(Expr::Call(9, _, Type::Int))]));
        parser.seek_scalar_offset(source.find("{ return compute(false)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved call error").err().unwrap();
        assert_eq!(error.message, "argument 1 to 'compute' must be i64");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.seek_scalar_offset(source.find("{ return imported_scalar").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved cross-module call").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Call(10, _, Type::Int), _)]));
        parser.seek_scalar_offset(source.find("{ return imported_private").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none());

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local array").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Return(Expr::Index(read, 2, index, ..), _)]
            if slot == read && values.len() == 2 && matches!(**index, Expr::Number(1))));
        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved array error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[1] = 9").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array store").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Store(read, 2, store_index, element, ..), Statement::Return(Expr::Index(read_back, 2, back_index, ..), _)]
            if slot == read && read == read_back && values.len() == 2
                && matches!(store_index, Expr::Number(1)) && matches!(element, Expr::Number(9))
                && matches!(**back_index, Expr::Number(1))));
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store index error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; a[0] = 9").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] = false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store value error").err().unwrap();
        assert_eq!(error.message, "array elements must be i64");
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] += 1").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none(), "compound store stays typed");

        let param_offset = source.find("(a: &[i64; 2]").unwrap() + 1;
        parser.names.insert("a".into(), (0, false, Type::Array(2)));
        parser.binding_ranges.insert(0, param_offset..param_offset + 1);
        parser.seek_scalar_offset(source.find("{ return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array param").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Index(slot, 2, index, ..), _)]
            if *slot == 0 && matches!(**index, Expr::Number(1))));
        let store_param_offset = source[param_offset + 1..].find("(a: &[i64; 2]").unwrap() + param_offset + 2;
        parser.binding_ranges.insert(0, store_param_offset..store_param_offset + 1);
        parser.seek_scalar_offset(source.find("{ a[0] = 3").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved param store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        let p_offset = source.find("(p: &Point)").unwrap() + 1;
        parser.names.insert("p".into(), (0, false, Type::StructRef(0)));
        parser.binding_ranges.insert(0, p_offset..p_offset + 1);
        parser.seek_scalar_offset(source.find("{ return p.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved field reads").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Binary(lhs, Kind::Plus, rhs, ..), _)]
            if matches!(&**lhs, Expr::Field(base, fname, fty)
                if matches!(&**base, Expr::Variable(0, Type::StructRef(0), ..)) && fname == "x" && *fty == Type::Int)
                && matches!(&**rhs, Expr::Field(_, fname, fty) if fname == "y" && *fty == Type::Int)));
        parser.names.remove("p");

        let h_offset = source.find("(h: &Holder)").unwrap() + 1;
        parser.names.insert("h".into(), (0, false, Type::StructRef(3)));
        parser.binding_ranges.insert(0, h_offset..h_offset + 1);
        parser.seek_scalar_offset(source.find("{ return h.text").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none(), "non-scalar field reads stay typed");
        parser.names.remove("h");

        let o_offset = source.find("(o: &Outer)").unwrap() + 1;
        parser.names.insert("o".into(), (0, false, Type::StructRef(2)));
        parser.binding_ranges.insert(0, o_offset..o_offset + 1);
        parser.seek_scalar_offset(source.find("{ return o.inner.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved nested field read").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Field(outer, fname, fty), _)]
            if matches!(&**outer, Expr::Field(inner, iname, ifty)
                if matches!(&**inner, Expr::Variable(0, Type::StructRef(2), ..)) && iname == "inner" && *ifty == Type::Struct(1))
                && fname == "x" && *fty == Type::Int));
        parser.names.remove("o");

        for prefix in [
            "choose[",
            "Pair[i64, vec[f64]] {",
            "Pair[i64, vec[f64]]::",
            "Choice[i64, vec[f64]]::Some",
            "Choice[i64, vec[f64]]::empty",
        ] {
            let target = source.find(prefix).unwrap();
            let ranges = parser.shared_specialization_arguments(target).unwrap();
            let spellings: Vec<_> = ranges.iter().map(|range| &source[range.clone()]).collect();
            assert_eq!(spellings, ["i64", "vec[f64]"]);
        }

        let target = source.find("*const shared").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < target);
        assert_eq!(parser.shared_pointer_cast_qualifier(), Some(false));
        assert_eq!(parser.pointer_cast_pointee(None).unwrap(), (false, Type::Shared(intern_shared(Type::Int))));
        assert_eq!(*parser.current(), Kind::Semi);

        for (prefix, expected) in [("|| -> i64", "i64"), ("|x: i64|", "bool")] {
            let offset = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < offset);
            let header = parser.shared_closure_header().unwrap();
            let ae_syntax::expression::ExpressionKind::Closure { params, return_type, .. } = &header.kind else {
                unreachable!("closure header index");
            };
            assert_eq!(&source[return_type.clone()], expected);
            if expected == "bool" {
                assert_eq!(params[0].name, "x");
                assert_eq!(&source[params[0].ty.clone()], "i64");
            } else {
                assert!(params.is_empty());
            }
        }

        parser.names.insert("flag".into(), (7, false, Type::Bool));
        parser.binding_ranges.insert(7, binding..binding + 4);
        parser.pos = initializer_token;
        assert!(matches!(
            parser.expression(0, true).unwrap(),
            (Expr::Variable(7, Type::Bool, _, _), Type::Bool),
        ));
        assert_eq!(*parser.current(), Kind::Semi);

        parser.pos = print_token + 2;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, print_token + 2);

        parser.pos = condition_token;
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::LBrace), false),
            Some((Expr::Variable(7, Type::Bool, _, _), Type::Bool)),
        ));
        assert_eq!(*parser.current(), Kind::LBrace);

        parser.pos = print_token;
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Variable(7, Type::Bool, _, _)))),
        ));

        parser.pos = print_token;
        parser.binding_ranges.insert(7, 0..4);
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, print_token);

        parser.binding_ranges.insert(7, binding..binding + 4);
        parser.pos = initializer_token;
        parser.expression_depth = 1;
        parser.expression_nodes = 127;
        assert!(matches!(
            parser.expression(0, false).unwrap(),
            (Expr::Variable(7, Type::Bool, _, _), Type::Bool),
        ));
        assert_eq!(parser.expression_nodes, 128);
        parser.pos = initializer_token;
        let error = match parser.expression(0, false) {
            Err(error) => error,
            Ok(_) => panic!("nested scalar exceeded the expression budget"),
        };
        assert_eq!(error.message, "expression complexity limit exceeded (128)");

        parser.pos = call_token;
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Call(9, _, Type::Int)))),
        ));
        parser.pos = call_token;
        parser.functions.get_mut("compute").unwrap().extern_c = true;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, call_token);

        parser.functions.insert("consume".into(), Signature {
            id: 10, params: vec![Type::String], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let binding = source.find("let text").unwrap() + 4;
        parser.names.insert("text".into(), (8, false, Type::String));
        parser.binding_ranges.insert(8, binding..binding + 4);
        parser.borrows.push((8, 0, BorrowKind::View));
        parser.pos = parser.tokens.iter().position(|token| token.offset == source.find("assert(consume").unwrap()).unwrap();
        let assertion = parser.pos;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, assertion);
        assert!(!parser.moved.contains(&8));
        assert!(parser.deferred_write_conflicts.is_empty());

        parser.pos = parser.tokens.iter().position(|token| token.offset == source.find("print(consume").unwrap()).unwrap();
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Call(10, _, Type::Int)))),
        ));
        assert!(parser.moved.contains(&8));
        let offset = source.find("print(consume(text").unwrap() + "print(consume(".len();
        assert_eq!(parser.deferred_write_conflicts, vec![(8, offset, "text".into(), "move", None)]);

        let vector_offset = source.find("vec[1.0").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < vector_offset);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::VecLit(values, Type::VecF, _, _), Type::VecF)) if values.len() == 2
        ));
        let print_offset = source.find("print(vec[1])").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < print_offset);
        let print = parser.pos;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, print);

        let binding = source.find("let v =").unwrap() + 4;
        parser.names.insert("v".into(), (11, false, Type::VecF));
        parser.binding_ranges.insert(11, binding..binding + 1);
        let index_print = source.find("print(v[0])").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < index_print);
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::VecIndex(11, _, 1, _, false, Type::F64))))
        ));

        let binding = source.find("var value").unwrap() + 4;
        parser.names.insert("value".into(), (12, true, Type::Bool));
        parser.binding_ranges.insert(12, binding..binding + 5);
        for (prefix, mutable) in [("&value", false), ("&mut value", true)] {
            let cast = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
            assert!(matches!(
                parser.lower_shared_value(Some(Kind::Semi), false),
                Some((Expr::BorrowAs(operand, actual, 1, column), ty))
                    if matches!(*operand, Expr::Variable(12, Type::Bool, _, _))
                        && actual == mutable && matches!(ty, Type::PtrMut(_)) == mutable
                        && column == cast + 1
            ));
            assert_eq!(*parser.current(), Kind::Semi);
        }

        parser.names.get_mut("value").unwrap().1 = false;
        let cast = source.find("&mut value").unwrap();
        let start = parser.tokens.partition_point(|token| token.offset < cast);
        parser.pos = start;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, start);

        let binding = source.find("let raw").unwrap() + 4;
        let raw = Type::Ptr(super::super::intern_pointee(Type::Bool));
        parser.names.insert("raw".into(), (13, false, raw));
        parser.binding_ranges.insert(13, binding..binding + 3);
        let cast = source.find("raw as").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), false),
            Some((Expr::PtrCast(operand, true, id, _, column), Type::PtrMut(pointee)))
                if matches!(*operand, Expr::Variable(13, ty, _, _) if ty == raw)
                    && id == pointee && super::super::ptr_pointee(id) == Type::F64
                    && column == cast + "raw as *mut f64;".len()
        ));
        assert_eq!(*parser.current(), Kind::Semi);
    }
}
