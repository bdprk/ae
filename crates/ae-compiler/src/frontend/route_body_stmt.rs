//! Statement admissibility for the resolved body route.
use super::*;
use super::route_marks::*;
use super::route_body_expr::*;

pub(super) fn compound_assignment_operator(operator: &Kind) -> Option<Kind> {
    match operator {
        Kind::PlusEqual => Some(Kind::Plus),
        Kind::MinusEqual => Some(Kind::Minus),
        Kind::StarEqual => Some(Kind::Star),
        Kind::SlashEqual => Some(Kind::Slash),
        Kind::PercentEqual => Some(Kind::Percent),
        _ => None,
    }
}

pub(super) fn exit_status_operand(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, arguments } = &value.kind else { return None };
    let [status] = arguments.as_slice() else { return None };
    matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if name == "exit").then_some(status)
}

/// A bare statement that only calls a name no declaration answers
/// (`missing_first();`): the typed route reports `unknown function 'name'`
/// (E0207) while parsing the callee, before the statement-level "must end with
/// '?'" check. The scan admits the shape so the lowering can reproduce it.
pub(super) fn bare_call_statement(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &value.kind else { return None };
    // The statement builtins have their own arms (they precede this one, and
    // a rejected shape of theirs must keep the typed route's message); any
    // other bare call — a function, a builtin value call (`find(..);`) or an
    // unknown name — is evaluated (borrow and move diagnostics included) and
    // then rejected with the typed route's "must end with '?'" rule.
    match &callee.kind {
        ExpressionKind::Atom(Kind::Ident(name)) => (!statement_builtin_name(name)).then_some(value),
        ExpressionKind::Field { .. }
        | ExpressionKind::Qualified { .. }
        | ExpressionKind::BracketedCallTarget { .. } => None,
        _ => Some(value),
    }
}

/// A name whose bare-call statement form has a dedicated arm in the body
/// lowering (`exit`, the vector and handle statement builtins, the spawn
/// forms, the yield). A bare call to one of these is never the generic
/// "must end with '?'" shape.
pub(super) fn statement_builtin_name(name: &str) -> bool {
    matches!(name,
        "exit" | "push" | "pop" | "sort" | "reverse"
        | "shared_set" | "chan_close" | "cell_store"
        | "task_spawn" | "scope_spawn" | "pool_spawn" | "task_yield")
}

pub(super) fn propagation_call(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: true } = &statement.kind else { return None };
    let ExpressionKind::Propagate(operand) = &value.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &operand.kind else { return None };
    match &callee.kind {
        ExpressionKind::Atom(Kind::Ident(name)) => (!RESERVED.contains(&name.as_str())
            // A result-returning builtin (`try_copy_file(..)?`) propagates
            // exactly like a user function: its error enum carries `?`.
            || (resolved_builtin(name) && ENUM_RESULT_BUILTINS.iter().any(|(builtin, _)| *builtin == name)))
            .then_some(operand),
        // A variant construction (`Option[i64]::Some(10)?`): the typed
        // expression parser builds the payload, `?` reads the enum result.
        ExpressionKind::Qualified { .. } => Some(operand),
        _ => None,
    }
}

pub(super) fn iterator_constructor(expression: &ae_syntax::expression::Expression) -> Option<Type> {
    use ae_syntax::expression::ExpressionKind;
    let ExpressionKind::Call { callee, arguments } = &expression.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    match (name.as_str(), arguments.len()) {
        ("tokenize", 2) => Some(Type::Tokens),
        ("strvec_new", 0) | ("lines", 1) | ("split", 2) => Some(Type::Vector(PRELUDE_STR_VEC_ELEM_ID)),
        _ => None,
    }
}

pub(super) fn scalar_body_statement(statement: &ae_syntax::statement::Statement, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool, supports_field: &impl Fn(&ae_syntax::expression::Expression, ae_resolve::Role) -> bool) -> bool {
    use ae_syntax::statement::{ElseBranch, StatementKind};
    match &statement.kind {
        StatementKind::Binding { name_range, initializer, .. } => {
            resolved.scalar_definitions.contains_key(&name_range.start)
                && match &initializer.kind {
                    ae_syntax::expression::ExpressionKind::Array(elements)
                    | ae_syntax::expression::ExpressionKind::Vector(elements) => elements.iter()
                        .all(|element| scalar_body_expression(element, resolved, supports_call, supports_field)
                            || matches!(&element.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&element.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id)))
                            || resolved.struct_field_views.contains(&element.range.start)),
                    ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)) => true,
                    // `let x = foo()?;` — a direct-call propagation binding. A result-returning
                    // builtin (`let a = try_read_file(first)?;`) propagates the
                    // same way; other RESERVED callees stay typed.
                    ae_syntax::expression::ExpressionKind::Propagate(operand) => {
                        matches!(&operand.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                            if matches!(&callee.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if !RESERVED.contains(&name.as_str())
                                    || (resolved_builtin(name) && ENUM_RESULT_BUILTINS.iter().any(|(builtin, _)| *builtin == name))))
                            // A variant construction (`let a = Option[i64]::Some(10)?;`).
                            || (matches!(&operand.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                                if matches!(callee.kind, ae_syntax::expression::ExpressionKind::Qualified { .. }))
                                && resolved.enum_constructors.contains(&operand.range.start))
                            // A checked integer method (`let v = a.checked_add(b)?;`).
                            || (matches!(&operand.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                                if matches!(&callee.kind, ae_syntax::expression::ExpressionKind::Field { name, .. }
                                    if IntMethodKind::from_name(name).is_some_and(IntMethodKind::is_checked)))
                                && scalar_body_expression(operand, resolved, supports_call, supports_field))
                    }
                    _ => iterator_constructor(initializer).is_some()
                        || resolved.enum_constructors.contains(&initializer.range.start)
                        || scalar_body_expression(initializer, resolved, supports_call, supports_field)
                        // A closure literal whose body the resolved route
                        // already collected: the lowering delegates the literal
                        // to the closure machinery (capture inference and the
                        // generated function).
                        || matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Closure { body, .. }
                            if resolved.scalar_bodies.contains_key(&body.start))
                        // A field read as the bound value: a scalar field is a
                        // copy, an owned one is the typed route's "cannot move
                        // a field out of a struct" rejection.
                        || matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Field { .. })
                        // `let copy = owned;` moves an owned value (the
                        // lowering marks the source moved; the view-after-
                        // last-use case defers through the move graph).
                        || matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&initializer.range.start)
                                .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    || resolved.owned_vectors.contains(id)
                                    || resolved.owned_struct_bindings.contains(id)
                                    // `let y = x;` moves an owned enum, which
                                    // the lowering records like any owned move.
                                    || resolved.enum_bindings.contains(id))),
                }
        }
        StatementKind::Assign { target, operator, value } => {
            match &target.kind {
                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) if name != "self" => {
                    resolved.scalar_writes.get(&target.range.start).is_some_and(|&id| {
                        if resolved.array_definitions.contains(&id)
                            || resolved.token_cursor_definitions.contains(&id)
                        {
                            return false;
                        }
                        // Reassigning an enum releases the old payload first;
                        // the value is a constructor or another owned enum.
                        if resolved.enum_bindings.contains(&id) {
                            return resolved.enum_constructors.contains(&value.range.start)
                                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(other))
                                    if other != "self" && resolved.scalar_reads.get(&value.range.start)
                                        .is_some_and(|other_id| resolved.enum_bindings.contains(other_id)))
                                // A call returning an enum (`e = swap(e)`): the
                                // callee's signature answers, and the lowering
                                // reports the same-statement move.
                                || scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        // Re-initializing an owned vector (after a move) takes
                        // a fresh vector: a literal or a call returning one.
                        if resolved.owned_vectors.contains(&id) {
                            return matches!(value.kind, ae_syntax::expression::ExpressionKind::Vector(_))
                                || scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        if resolved.vector_definitions.contains(&id) {
                            return false;
                        }
                        if !resolved.owned_text_bindings.contains(&id) {
                            return scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        // Reassigning an owned string: a fresh literal or
                        // another owned string moves in, and the lowering
                        // releases the old value (the Assign codegen's flag).
                        matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(other))
                                if other != "self" && resolved.scalar_reads.get(&value.range.start)
                                    .is_some_and(|other_id| other_id != &id
                                        && (resolved.owned_text_bindings.contains(other_id)
                                            || resolved.owned_string_bindings.contains(other_id))))
                            // A fresh string from a value builtin: the
                            // assignment releases the old value and takes the
                            // new one.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                                if matches!(&callee.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                    if string_result_builtin(name)))
                            // A concat always builds a fresh string, so
                            // `s = a + b` releases the old value and takes
                            // the new one.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Binary { operator: Kind::Plus, .. })
                            // Any other bare name: the lowering's assignment
                            // type check answers (a string view, a scalar, an
                            // aggregate), positioned at the target name like
                            // the typed route.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                    })
                }
                // `a[i] = v` stores through a resolved array binding. Only
                // the plain operator: a compound store stays on the typed
                // route, which rejects it for arrays.
                ae_syntax::expression::ExpressionKind::Index { base, index }
                    if *operator == Kind::Equal =>
                {
                    // A named container, or a field of a name (`p.table[k]`):
                    // the field's declared type comes from the struct table, so
                    // a field base needs no container mark on the root.
                    let field_base = matches!(base.kind, ae_syntax::expression::ExpressionKind::Field { .. })
                        && field_chain_root(base).is_some();
                    (matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))) || field_base)
                        && (field_base
                            || resolved.scalar_writes.get(&base.range.start)
                                .or_else(|| resolved.scalar_reads.get(&base.range.start))
                                .is_some_and(|id| resolved.array_definitions.contains(id)
                                    || resolved.map_definitions.contains(id)
                                    // A vector store (`v[i] = x`): the lowering
                                    // checks mutability, the element type, and the
                                    // string move for a view/value store.
                                    || resolved.vector_definitions.contains(id)))
                        // A map key is a string value or view; array indices
                        // are scalars.
                        && (scalar_body_expression(index, resolved, supports_call, supports_field)
                            || matches!(index.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&index.range.start))
                        && (scalar_body_expression(value, resolved, supports_call, supports_field)
                            || matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&value.range.start)
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&value.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id)
                                        // A nested vector or an owned struct
                                        // moves into the slot.
                                        || resolved.owned_vectors.contains(id)
                                        || resolved.owned_struct_bindings.contains(id))))
                }
                ae_syntax::expression::ExpressionKind::Field { .. }
                    if (*operator == Kind::Equal || compound_assignment_operator(operator).is_some())
                        && field_chain_root(target).is_some() =>
                {
                    supports_field(target, ae_resolve::Role::Write)
                        && (scalar_body_expression(value, resolved, supports_call, supports_field)
                            // An owned aggregate moves into the field
                            // (`r.value = replacement`), like a struct literal
                            // field: the lowering records the move.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&value.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id)
                                        || resolved.owned_vectors.contains(id)
                                        || resolved.owned_struct_bindings.contains(id)
                                        || resolved.enum_bindings.contains(id)
                                        || resolved.map_definitions.contains(id))))
                }
                _ => false,
            }
        }
        StatementKind::Return(value) => {
            // A returned closure literal has the typed route's own diagnostic
            // (a closure has no storage); every other value keeps the shaped
            // gates.
            matches!(&value.kind, ae_syntax::expression::ExpressionKind::Closure { .. })
                || scalar_body_expression(value, resolved, supports_call, supports_field)
                || resolved.enum_constructors.contains(&value.range.start)
                || receiver_value(value).is_some_and(|atom| supports_field(atom, ae_resolve::Role::Read))
                // `return binding;` moves an owned struct out of the body;
                // the lowering marks the slot moved, so the drops pass trims
                // its Frees instead of releasing a value the caller owns.
                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&value.range.start)
                        .is_some_and(|id| resolved.owned_struct_bindings.contains(id)
                            || resolved.owned_vectors.contains(id)
                            // `return owned_string;` moves the string out the
                            // same way.
                            || resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)
                            // `return counts;` for a map the body owns.
                            || resolved.map_definitions.contains(id)
                            // `return owned_enum;` moves the payload out.
                            || resolved.enum_bindings.contains(id)))
        }
        // A literal print is the typed route's `Printed::Text` (no expression
        // to lower); a string field prints as its view; everything else must
        // resolve as a printable scalar.
        StatementKind::Print(value) => {
            // Any known local reaches the print arm's own type check
            // (`cannot print a value of type ...`); the arm admits the
            // printable shapes and rejects the rest exactly as the typed route.
            matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                if name != "self" && resolved.scalar_reads.contains_key(&value.range.start))
                || matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                || (matches!(value.kind, ae_syntax::expression::ExpressionKind::Field { .. })
                    && resolved.struct_field_views.contains(&value.range.start))
                // Printing an owned string reads a view of it; nothing moves.
                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&value.range.start)
                        .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)))
                || scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::Assert(value) => {
            scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::PointerStore { pointer_range, value, .. } => {
            resolved.scalar_writes.contains_key(&pointer_range.start)
                && scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::If { condition, then_body, else_body } => {
            scalar_body_expression(condition, resolved, supports_call, supports_field)
                && then_body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
                && match else_body {
                    Some(ElseBranch::Block(body)) => body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field)),
                    Some(ElseBranch::If(branch)) => scalar_body_statement(branch, resolved, supports_call, supports_field),
                    None => true,
                }
        }
        StatementKind::While { condition, body } => {
            scalar_body_expression(condition, resolved, supports_call, supports_field)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::ForRange { binding_range, start, end, body, .. } => {
            resolved.scalar_definitions.contains_key(&binding_range.start)
                && scalar_body_expression(start, resolved, supports_call, supports_field)
                && scalar_body_expression(end, resolved, supports_call, supports_field)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::ForIn { binding_range, iterable, body, .. } => {
            resolved.scalar_definitions.contains_key(&binding_range.start)
                // A bare local is the fast path. A call iterable (a temporary
                // cursor, `for t in tokenize(..)`) is admitted so the
                // lowering's loop check reports the typed route's own
                // diagnostic instead of denying the body.
                && matches!(iterable.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))
                    | ae_syntax::expression::ExpressionKind::Call { .. })
                && (matches!(iterable.kind, ae_syntax::expression::ExpressionKind::Call { .. })
                    || resolved.scalar_reads.get(&iterable.range.start)
                        .is_some_and(|id| resolved.vector_definitions.contains(id) || resolved.token_cursor_definitions.contains(id))
                    // A slice parameter (or a local the marks do not
                    // classify) iterates permissively: the lowering's loop
                    // check answers by the slot's type.
                    || resolved.scalar_reads.contains_key(&iterable.range.start))
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::Match { subject, arms } => {
            // A bare local subject is admitted permissively: the lowering's
            // `lower_match` reports the typed route's "match needs a bare enum
            // variable as its subject" for a non-enum and reads the enum's own
            // layout for a genuine one (`match self` included).
            matches!(subject.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                && arms.iter().all(|arm| arm.body.iter().all(|statement| {
                    scalar_body_statement(statement, resolved, supports_call, supports_field)
                }))
        }
        StatementKind::Unsafe(body) => body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field)),
        StatementKind::Break | StatementKind::Continue => true,
        StatementKind::Expr { .. } => exit_status_operand(statement)
            .is_some_and(|status| scalar_body_expression(status, resolved, supports_call, supports_field))
            || propagation_call(statement).is_some()
            || bare_call_statement(statement).is_some()
            || is_task_yield(statement)
            || handle_statement_builtin(statement).is_some()
            || vector_statement_builtin(statement, resolved)
                .is_some_and(|(builtin, value)| matches!(builtin, "pop" | "sort" | "reverse")
                    || scalar_body_expression(value, resolved, supports_call, supports_field)
                    || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                    || resolved.struct_field_views.contains(&value.range.start)
                    || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&value.range.start)
                            .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                || resolved.owned_string_bindings.contains(id)
                                // A nested vector or an owned struct moves
                                // into the container.
                                || resolved.owned_vectors.contains(id)
                                || resolved.owned_struct_bindings.contains(id)))),
    }
}

/// A `push`/`pop`/`sort`/`reverse` statement over an owned vector binding or a
/// struct's vector field: (the builtin's name, the pushed value expression
/// when it has one). Shape only — the lowering enforces mutability, element
/// types and borrow rules.
pub(super) fn vector_statement_builtin<'a>(statement: &'a ae_syntax::statement::Statement, resolved: &ResolvedExpressions) -> Option<(&'static str, &'a ae_syntax::expression::Expression)> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, arguments } = &value.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    let builtin = match name.as_str() {
        "push" => "push", "pop" => "pop", "sort" => "sort", "reverse" => "reverse",
        _ => return None,
    };
    // A struct field receiver (`push(r.cells, x)`): the lowering resolves the
    // field's declared type, so the gate only checks the shape.
    // A struct field chain receiver (`push(r.cells, x)`, `push(a.b.cells, x)`):
    // the lowering resolves the declared field type, so the gate checks only
    // that the chain roots at a binding.
    let field_target = arguments.first().is_some_and(|argument| {
        matches!(argument.kind, ExpressionKind::Field { .. }) && field_chain_root(argument).is_some()
    });
    if !field_target {
        let Some(ExpressionKind::Atom(Kind::Ident(_))) = arguments.first().map(|argument| &argument.kind) else { return None };
        let def = resolved.scalar_reads.get(&arguments[0].range.start)?;
        // An owned vector takes the statement; a borrowed one (`&vec`), a
        // slice parameter, or any other known local is admitted too so the
        // lowering reports the typed route's own shape diagnostic
        // ('push expects a vec, found &[i64]'), never a denial.
        if !resolved.vector_definitions.contains(def) && !resolved.scalar_reads.contains_key(&arguments[0].range.start) {
            return None;
        }
    }
    match (builtin, arguments.len()) {
        ("push", 2) => Some((builtin, &arguments[1])),
        ("pop" | "sort" | "reverse", 1) => Some((builtin, &arguments[0])),
        _ => None,
    }
}

/// A handle builtin written as a statement (`shared_set(m, v);`,
/// `chan_close(ch);`, `cell_store(h, v);`): these lower directly to their
/// statements, so the expression form of the same name is never consulted.
pub(super) fn handle_statement_builtin(statement: &ae_syntax::statement::Statement) -> Option<(&'static str, &ae_syntax::expression::Expression)> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &value.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    let builtin = match name.as_str() {
        "shared_set" => "shared_set", "chan_close" => "chan_close", "cell_store" => "cell_store",
        "task_spawn" => "task_spawn", "scope_spawn" => "scope_spawn", "pool_spawn" => "pool_spawn",
        _ => return None,
    };
    Some((builtin, value))
}

/// `task_yield();` — a suspension point with no operands. Its statement exists
/// in the lowered form already (`Statement::TaskYield`).
pub(super) fn is_task_yield(statement: &ae_syntax::statement::Statement) -> bool {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return false };
    matches!(&value.kind, ExpressionKind::Call { callee, arguments }
        if arguments.is_empty()
            && matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if name == "task_yield"))
}
