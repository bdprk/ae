//! The control-flow layer: each function becomes a graph of blocks whose
//! contents are the statement tree's own statements, verbatim (M2.2 slice 2).
//!
//! Only control flow is lowered: `if`/`while`/`for`/`match`/`return`/`exit`
//! shape the graph, `break`/`continue` become explicit edges, and every other
//! statement — assignments, calls, pushes, releases — is kept whole inside
//! its block. This is the flow-sensitive base the borrow checker needs: move
//! and borrow states are computed per block along these edges. Expressions
//! stay unsplit; `&&`/`||` keep their source form as whole conditions, and
//! `?`/`assert` keep their whole-statement form because their branch lives
//! inside the operation.
//!
//! Printed by `ae emit-cfg`; the validator checks that every reachable
//! block ends in exactly one terminator and that all branch targets exist.
//!
//! Deadness is tracked per statement list: a list whose every path ends
//! unconditionally (return/exit/break/continue) reports that upward, so the
//! statements after it — the tree's function-tail releases a return edge
//! already carries — are dropped instead of being placed in a block that
//! never terminates. Control statements with a continuation (if/while/for/
//! match) report "alive" through their join as long as some arm continues.
use super::*;

pub(super) fn cfg_path(path: &Path) -> Result<String, String> {
    cfg_path_with_imports(path, &PackageImports::new())
}

pub(super) fn cfg_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    let program = loader::load_program_with_imports(path, imports)?;
    let mut out = String::new();
    for file in &program.files {
        for function in file {
            let graph = build_cfg(function, &program.ctx);
            validate(&graph);
            print_function(&graph, &mut out, &program.ctx);
        }
    }
    Ok(out)
}

#[derive(Debug)]
pub(super) struct CfgFunction {
    pub(super) id: usize,
    pub(super) is_main: bool,
    pub(super) params: Vec<(usize, Type)>,
    pub(super) returns: Type,
    pub(super) blocks: Vec<CfgBlock>,
}

#[derive(Debug)]
pub(super) struct CfgBlock {
    pub(super) id: usize,
    pub(super) statements: Vec<Statement>,
    pub(super) terminator: CfgTerminator,
}

#[derive(Debug, Clone)]
pub(super) enum CfgTerminator {
    // Unconditional edge to another block.
    Goto(usize),
    // Two-way branch; the condition is the source expression, unsplit.
    Branch(Expr, usize, usize),
    // Enum dispatch: the subject variable id, nested field path, its layout id,
    // `(variant, target)` pairs, and an optional default target for wildcards.
    Switch(usize, Vec<(usize, usize)>, usize, Vec<(usize, usize)>, Option<usize>),
    Return(Expr),
    Exit(Expr),
    // A block that no path reaches: the lowering's pending tail, or the
    // continuation every path of a fully-returning list abandoned.
    Unreachable,
}

struct Lowering<'a> {
    ctx: &'a CompileContext,
    blocks: Vec<CfgBlock>,
    next_block: usize,
    next_synthetic: usize,
    // Loop stacks: break targets the exit; continue targets the block that
    // re-enters the loop's check — the head for `while`/`for .. in tokens`,
    // the step block (hidden increment) for a counted `for` (M2.3).
    loop_exits: Vec<usize>,
    loop_continues: Vec<usize>,
}

impl<'a> Lowering<'a> {
    fn new(ctx: &'a CompileContext) -> Self {
        Self {
            ctx,
            blocks: Vec::new(),
            next_block: 0,
            next_synthetic: 0,
            loop_exits: Vec::new(),
            loop_continues: Vec::new(),
        }
    }
    fn synthetic_local(&mut self) -> usize {
        let id = usize::MAX - self.next_synthetic;
        self.next_synthetic += 1;
        id
    }

    fn new_block(&mut self) -> usize {
        let id = self.next_block;
        self.next_block += 1;
        self.blocks.push(CfgBlock { id, statements: Vec::new(), terminator: CfgTerminator::Unreachable });
        id
    }
    fn block(&mut self, id: usize) -> &mut CfgBlock {
        &mut self.blocks[id]
    }
}

pub(super) fn build_cfg(function: &Function, ctx: &CompileContext) -> CfgFunction {
    let mut lowering = Lowering::new(ctx);
    let entry = lowering.new_block();
    lower_statements(&function.body, entry, &mut lowering);
    CfgFunction { id: function.id, is_main: function.is_main, params: function.params.clone(), returns: function.returns, blocks: lowering.blocks }
}

// Lower a statement list into `entry`. The bool answers "did every path
// through this list end unconditionally (return/exit/break/continue)?" —
// when true, the block the list nominally continues in is unreachable, and
// statements after the list (the tree's function-tail releases) are dead.
// Statements *within* the list after such an end are dropped by the loop.
fn lower_statements(statements: &[Statement], entry: usize, lowering: &mut Lowering) -> (usize, bool) {
    let mut current = entry;
    let mut ended = false;
    for statement in statements {
        if ended { break; }
        let (next, list_ended) = lower_statement(statement, current, lowering);
        current = next;
        ended = list_ended;
    }
    (current, ended)
}

fn lower_condition(condition: &Expr, entry: usize, yes: usize, no: usize, lowering: &mut Lowering) {
    match condition {
        Expr::Binary(left, Kind::And, right, ..) => {
            let rhs = lowering.new_block();
            lower_condition(left, entry, rhs, no, lowering);
            lower_condition(right, rhs, yes, no, lowering);
        }
        Expr::Binary(left, Kind::Or, right, ..) => {
            let rhs = lowering.new_block();
            lower_condition(left, entry, yes, rhs, lowering);
            lower_condition(right, rhs, yes, no, lowering);
        }
        Expr::Not(inner) => lower_condition(inner, entry, no, yes, lowering),
        other => lowering.block(entry).terminator = CfgTerminator::Branch(other.clone(), yes, no),
    }
}

fn lower_boolean_value(condition: &Expr, current: usize, lowering: &mut Lowering) -> (usize, Expr) {
    let result = lowering.synthetic_local();
    lowering.block(current).statements.push(Statement::Declare(result, Expr::BoolLit(false)));
    let yes = lowering.new_block();
    let join = lowering.new_block();
    lower_condition(condition, current, yes, join, lowering);
    let taken = lowering.block(yes);
    taken.statements.push(Statement::Assign(result, Expr::BoolLit(true), false));
    taken.terminator = CfgTerminator::Goto(join);
    (join, Expr::Variable(result, Type::Bool, 0, 0))
}

fn is_short_circuit(expression: &Expr) -> bool {
    match expression {
        Expr::Binary(_, Kind::And | Kind::Or, ..) => true,
        Expr::Not(operand) => is_short_circuit(operand),
        _ => false,
    }
}

fn lower_exit_operand(expr: &Expr, current: usize, lowering: &mut Lowering) -> (usize, Expr) {
    match expr {
        Expr::Number(_) | Expr::BoolLit(_) | Expr::FloatLit(_) | Expr::CharLit(_) | Expr::Unit | Expr::Variable(..) => {
            (current, expr.clone())
        }
        _ if is_short_circuit(expr) => lower_boolean_value(expr, current, lowering),
        _ => {
            let result = lowering.synthetic_local();
            lowering.block(current).statements.push(Statement::Declare(result, expr.clone()));
            (current, Expr::Variable(result, super::mir::expr_type(expr, lowering.ctx), 0, 0))
        }
    }
}

// Lower one statement; returns the continuation block and whether every
// path through it ended unconditionally.
fn lower_statement(statement: &Statement, current: usize, lowering: &mut Lowering) -> (usize, bool) {
    match statement {
        Statement::Declare(id, value) if is_short_circuit(value) => {
            let (join, value) = lower_boolean_value(value, current, lowering);
            lowering.block(join).statements.push(Statement::Declare(*id, value));
            (join, false)
        }
        Statement::Assign(id, value, release_old) if is_short_circuit(value) => {
            let (join, value) = lower_boolean_value(value, current, lowering);
            lowering.block(join).statements.push(Statement::Assign(*id, value, *release_old));
            (join, false)
        }
        Statement::If(cond, yes, no) => {
            let then_block = lowering.new_block();
            let else_block = lowering.new_block();
            lower_condition(cond, current, then_block, else_block, lowering);
            let (after_then, ended_then) = lower_statements(yes, then_block, lowering);
            let (after_else, ended_else) = lower_statements(no, else_block, lowering);
            let joined = join(after_then, after_else, lowering);
            // The join is a continuation only when some arm continues.
            (joined, ended_then && ended_else)
        }
        Statement::While(cond, body) => {
            // The head is a dedicated block holding only the condition's
            // branch. Making `current` the head would send every `continue`
            // — and the body's back edge — through the statements *before*
            // the loop, re-running them each iteration (a `var i = 0` in the
            // same block would reset the counter forever).
            let head = lowering.new_block();
            lowering.block(current).terminator = CfgTerminator::Goto(head);
            let body_block = lowering.new_block();
            let exit = lowering.new_block();
            lower_condition(cond, head, body_block, exit, lowering);
            lowering.loop_exits.push(exit);
            lowering.loop_continues.push(head);
            let (after_body, _) = lower_statements(body, body_block, lowering);
            lowering.loop_exits.pop();
            lowering.loop_continues.pop();
            redirect(after_body, head, lowering);
            // The exit is a continuation whatever the body did.
            (exit, false)
        }
        Statement::For(var, end_var, body, _label, line, col) => {
            // A counted loop lowers into its full shape (M2.3): a head whose
            // branch is the real `counter < limit` test, a body, and a step
            // block that carries the hidden checked increment — the landing
            // `continue` targets, matching the tree's `ae_next_{label}`.
            // The parser declares both slots as i64 and bounds evaluate once
            // before the loop, so the head re-reads them each iteration.
            let head = lowering.new_block();
            lowering.block(current).terminator = CfgTerminator::Goto(head);
            let body_block = lowering.new_block();
            let step = lowering.new_block();
            let exit = lowering.new_block();
            let condition = Expr::Binary(
                Box::new(Expr::Variable(*var, Type::Int(IntTy::I64), *line, *col)),
                Kind::Less,
                Box::new(Expr::Variable(*end_var, Type::Int(IntTy::I64), *line, *col)),
                *line,
                *col,
                Type::Bool,
            );
            lowering.block(head).terminator = CfgTerminator::Branch(condition, body_block, exit);
            lowering.loop_exits.push(exit);
            lowering.loop_continues.push(step);
            let (after_body, _) = lower_statements(body, body_block, lowering);
            lowering.loop_exits.pop();
            lowering.loop_continues.pop();
            redirect(after_body, step, lowering);
            // The increment is the tree's own statement shape: an Assign of
            // `counter + 1` at the For's position, so the checked-add
            // diagnostic points at the loop header like the tree backend's.
            let increment = Expr::Binary(
                Box::new(Expr::Variable(*var, Type::Int(IntTy::I64), *line, *col)),
                Kind::Plus,
                Box::new(Expr::Number(1)),
                *line,
                *col,
                Type::Int(IntTy::I64),
            );
            lowering.block(step).statements.push(Statement::Assign(*var, increment, false));
            lowering.block(step).terminator = CfgTerminator::Goto(head);
            (exit, false)
        }
        Statement::ForTokens(cursor, element, body, ..) => {
            let iteration = lowering.synthetic_local();
            lowering.block(current).statements.push(Statement::ResetTokens(*cursor, iteration));
            let head = lowering.new_block();
            lowering.block(current).terminator = CfgTerminator::Goto(head);
            let body_block = lowering.new_block();
            let exit = lowering.new_block();
            let advanced = lowering.synthetic_local();
            lowering.block(head).statements.push(Statement::AdvanceTokens(*cursor, iteration, *element, advanced));
            let has_element = Expr::Variable(advanced, Type::Bool, 0, 0);

            lowering.block(head).terminator = CfgTerminator::Branch(has_element, body_block, exit);
            lowering.loop_exits.push(exit);
            lowering.loop_continues.push(head);
            let (after_body, _) = lower_statements(body, body_block, lowering);
            lowering.loop_exits.pop();
            lowering.loop_continues.pop();
            redirect(after_body, head, lowering);
            (exit, false)
        }
        Statement::Match(subject, ty, arms) => {
            if !matches!(*ty, Type::Enum(_, _)) {
                // The statement tree guarantees an enum subject; the arm is a
                // defensive default that the validator would flag.
                return (current, false);
            }
            let mut arm_blocks = Vec::with_capacity(arms.len());
            let mut joins = Vec::new();
            let mut all_ended = true;
            for arm in arms {
                let block = lowering.new_block();
                for (bid, pty, path) in &arm.bindings {
                    lowering.block(block).statements.push(Statement::BindPayload(
                        *bid,
                        *subject,
                        path.clone(),
                        *pty,
                    ));
                }
                let (after, ended) = lower_statements(&arm.body, block, lowering);
                arm_blocks.push(block);
                joins.push((after, ended));
                all_ended = all_ended && ended;
            }
            let rows: Vec<MatchRow<'_>> = arms
                .iter()
                .enumerate()
                .map(|(idx, arm)| MatchRow {
                    tests: vec![(Vec::new(), &arm.pattern)],
                    arm_block: arm_blocks[idx],
                })
                .collect();
            lower_match_matrix(*subject, &rows, current, lowering);
            // Every continuing arm converges here, like an `if`'s join: code
            // after the match — and the enclosing loop's back edge — lives in
            // the join block, not in whichever arm happened to be first.
            let joined = join_all(&joins, lowering);
            (joined, all_ended)
        }
        Statement::Propagate(value, ty, fields, binding, frees) => {
            let Type::Enum(_, layout) = *ty else { unreachable!("propagation requires an enum") };
            // Source locals use dense increasing IDs. Compiler-generated slots
            // occupy the opposite end of the ID space.
            let subject = lowering.synthetic_local();
            let success = lowering.new_block();
            let failure = lowering.new_block();
            let entry = lowering.block(current);
            entry.statements.push(Statement::Declare(subject, value.clone()));
            entry.terminator = CfgTerminator::Switch(
                subject,
                Vec::new(),
                layout,
                vec![(0, success), (1, failure)],
                None,
            );
            let failed = lowering.block(failure);
            if !frees.is_empty() {
                failed.statements.push(Statement::Frees(frees.clone()));
            }
            failed.terminator = CfgTerminator::Return(Expr::Variable(subject, *ty, 0, 0));
            lowering.block(success).statements.push(
                Statement::TakePropagationPayload(subject, *ty, fields.clone(), *binding),
            );
            (success, false)
        }
        Statement::Print(Printed::Expr(value)) if is_short_circuit(value) => {
            let (join, value) = lower_boolean_value(value, current, lowering);
            lowering.block(join).statements.push(Statement::Print(Printed::Expr(value)));
            (join, false)
        }
        Statement::Assert(value, line, column) if is_short_circuit(value) => {
            let (join, value) = lower_boolean_value(value, current, lowering);
            lowering.block(join).statements.push(Statement::Assert(value, *line, *column));
            (join, false)
        }
        Statement::Return(expr, frees) => {
            let (current, value) = lower_exit_operand(expr, current, lowering);
            // The release annotations ride the return: they are statements
            // the lowering keeps in order, then the return edge.
            if !frees.is_empty() {
                lowering.block(current).statements.push(Statement::Frees(frees.clone()));
            }
            lowering.block(current).terminator = CfgTerminator::Return(value);
            (lowering.new_block(), true)
        }
        Statement::Exit(code, frees) => {
            let (current, value) = lower_exit_operand(code, current, lowering);
            if !frees.is_empty() {
                lowering.block(current).statements.push(Statement::Frees(frees.clone()));
            }
            lowering.block(current).terminator = CfgTerminator::Exit(value);
            (lowering.new_block(), true)
        }
        // An `unsafe` block is a scoping container (M2.12 slice 1): its inner
        // statements lower inline, like the branches of an `if` minus the
        // branch, so the graph and every pass over it see flat statements.
        Statement::Unsafe(stmts) => {
            let (after, ended) = lower_statements(stmts, current, lowering);
            (after, ended)
        }
        Statement::Break => {
            let Some(exit) = lowering.loop_exits.last().copied() else {
                // The parser rejects break outside loops; the arm is a guard.
                lowering.block(current).terminator = CfgTerminator::Unreachable;
                return (lowering.new_block(), true);
            };
            lowering.block(current).terminator = CfgTerminator::Goto(exit);
            (lowering.new_block(), true)
        }
        Statement::Continue(depth) => {
            let index = match depth { Some(d) if *d > 0 && *d <= lowering.loop_continues.len() => lowering.loop_continues.len() - *d, _ => lowering.loop_continues.len().saturating_sub(1) };
            let Some(head) = lowering.loop_continues.get(index).copied() else {
                lowering.block(current).terminator = CfgTerminator::Unreachable;
                return (lowering.new_block(), true);
            };
            lowering.block(current).terminator = CfgTerminator::Goto(head);
            (lowering.new_block(), true)
        }
        // Every other non-control statement stays whole in the current block.
        other => {
            lowering.block(current).statements.push(other.clone());
            (current, false)
        }
    }
}

struct MatchRow<'a> {
    tests: Vec<(Vec<(usize, usize)>, &'a CompiledPattern)>,
    arm_block: usize,
}

fn lower_match_matrix(subject: usize, rows: &[MatchRow<'_>], current: usize, lowering: &mut Lowering) {
    let Some(first) = rows.first() else {
        return;
    };
    let Some(test_pos) = first
        .tests
        .iter()
        .position(|(_, pat)| matches!(pat, CompiledPattern::Variant { .. }))
    else {
        lowering.block(current).terminator = CfgTerminator::Goto(first.arm_block);
        return;
    };
    let (ref test_path, CompiledPattern::Variant { eid, .. }) = first.tests[test_pos] else {
        unreachable!();
    };
    let test_path = test_path.clone();
    let eid = *eid;

    let mut variants_seen = Vec::new();
    for row in rows {
        if let Some((_, CompiledPattern::Variant { variant, .. })) = row
            .tests
            .iter()
            .find(|(p, _)| *p == test_path)
            && !variants_seen.contains(variant)
        {
            variants_seen.push(*variant);
        }
    }

    let mut targets = Vec::with_capacity(variants_seen.len());
    for v in &variants_seen {
        let mut sub_rows = Vec::new();
        for row in rows {
            if let Some(pos) = row.tests.iter().position(|(p, _)| *p == test_path) {
                match row.tests[pos].1 {
                    CompiledPattern::Variant { variant, fields, .. } if variant == v => {
                        let mut new_tests = row.tests.clone();
                        new_tests.remove(pos);
                        let mut insert_at = pos;
                        for (fidx, _, subpat) in fields {
                            if matches!(subpat, CompiledPattern::Variant { .. }) {
                                let mut sub_path = test_path.clone();
                                sub_path.push((*v, *fidx));
                                new_tests.insert(insert_at, (sub_path, subpat));
                                insert_at += 1;
                            }
                        }
                        sub_rows.push(MatchRow { tests: new_tests, arm_block: row.arm_block });
                    }
                    CompiledPattern::Variant { .. } => {}
                    CompiledPattern::Wildcard | CompiledPattern::Binding => {
                        let mut new_tests = row.tests.clone();
                        new_tests.remove(pos);
                        sub_rows.push(MatchRow { tests: new_tests, arm_block: row.arm_block });
                    }
                }
            } else {
                sub_rows.push(MatchRow { tests: row.tests.clone(), arm_block: row.arm_block });
            }
        }
        let target = if let Some(first_sub) = sub_rows.first()
            && !first_sub.tests.iter().any(|(_, pat)| matches!(pat, CompiledPattern::Variant { .. }))
        {
            first_sub.arm_block
        } else {
            let dispatch = lowering.new_block();
            lower_match_matrix(subject, &sub_rows, dispatch, lowering);
            dispatch
        };
        targets.push((*v, target));
    }

    let mut default_rows = Vec::new();
    for row in rows {
        let matches_default = row
            .tests
            .iter()
            .find(|(p, _)| *p == test_path)
            .is_none_or(|(_, pat)| !matches!(pat, CompiledPattern::Variant { .. }));
        if matches_default {
            let mut new_tests = row.tests.clone();
            new_tests.retain(|(p, _)| *p != test_path);
            default_rows.push(MatchRow { tests: new_tests, arm_block: row.arm_block });
        }
    }
    let default_target = if default_rows.is_empty() {
        None
    } else if let Some(first_def) = default_rows.first()
        && !first_def.tests.iter().any(|(_, pat)| matches!(pat, CompiledPattern::Variant { .. }))
    {
        Some(first_def.arm_block)
    } else {
        let dispatch = lowering.new_block();
        lower_match_matrix(subject, &default_rows, dispatch, lowering);
        Some(dispatch)
    };

    lowering.block(current).terminator = CfgTerminator::Switch(subject, test_path, eid, targets, default_target);
}

// Redirect two paths into one continuation block; identical paths stay one.
fn join(a: usize, b: usize, lowering: &mut Lowering) -> usize {
    if a == b { return a; }
    let join = lowering.new_block();
    redirect(a, join, lowering);
    redirect(b, join, lowering);
    join
}

// Merge a set of path continuations into one block.
fn join_all(joins: &[(usize, bool)], lowering: &mut Lowering) -> usize {
    let mut blocks: Vec<usize> = joins.iter().map(|(block, _)| *block).collect();
    let Some(first) = blocks.first().copied() else { return lowering.new_block(); };
    let mut joined = first;
    for next in blocks.drain(1..) {
        joined = join(joined, next, lowering);
    }
    joined
}

// Point a block's fall-through at `target`; terminated blocks keep theirs.
fn redirect(from: usize, target: usize, lowering: &mut Lowering) {
    if let CfgTerminator::Unreachable = lowering.block(from).terminator {
        lowering.block(from).terminator = CfgTerminator::Goto(target);
    }
}

// The graph's invariants: one terminator per block and targets that exist.
// A violation is a lowering bug, not a user error -- the message says
// `internal`. A block with statements and no terminator is legal only in
// `main`, whose implicit exit-0 the tree does not spell out.
fn validate(function: &CfgFunction) {
    let count = function.blocks.len();
    for block in &function.blocks {
        match &block.terminator {
            CfgTerminator::Unreachable => {
                if !block.statements.is_empty() && !function.is_main {
                    if std::env::var_os("AE_DEBUG_CFG").is_some() {
                        eprintln!("cfg debug: fn #{} block {} holds {} statements:", function.id, block.id, block.statements.len());
                        for other in &function.blocks {
                            let shape = match &other.terminator {
                                CfgTerminator::Goto(t) => format!("goto {}", t),
                                CfgTerminator::Branch(_, a, b) => format!("branch {} {}", a, b),
                                CfgTerminator::Switch(_, _, _, ts, def) => format!("switch {:?} default {:?}", ts, def),
                                CfgTerminator::Return(_) => "return".to_owned(),
                                CfgTerminator::Exit(_) => "exit".to_owned(),
                                CfgTerminator::Unreachable => "unreachable".to_owned(),
                            };
                            eprintln!("  block {} -> {} ({} statements)", other.id, shape, other.statements.len());
                        }
                    }
                    panic!("internal: block {} of fn #{} holds statements but never terminates", block.id, function.id);
                }
            }
            CfgTerminator::Goto(target) => assert!(*target < count, "internal: fn #{} block {} branches to missing block {}", function.id, block.id, target),
            CfgTerminator::Branch(_, a, b) => assert!(*a < count && *b < count, "internal: fn #{} block {} branches to missing block", function.id, block.id),
            CfgTerminator::Switch(_, _, _, targets, default) => {
                for (_, t) in targets {
                    assert!(*t < count, "internal: fn #{} switch target missing", function.id);
                }
                if let Some(d) = default {
                    assert!(*d < count, "internal: fn #{} switch default target missing", function.id);
                }
            }
            CfgTerminator::Return(_) | CfgTerminator::Exit(_) => {}
        }
    }
}

fn print_function(function: &CfgFunction, out: &mut String, ctx: &CompileContext) {
    let name = if function.is_main { "main".to_owned() } else { format!("#{}", function.id) };
    let params = function.params.iter().map(|(id, t)| format!("v{}: {}", id, t.name(ctx))).collect::<Vec<_>>().join(", ");
    out.push_str(&format!("fn {}({}) -> {} ({} blocks)\n", name, params, function.returns.name(ctx), function.blocks.len()));
    for block in &function.blocks {
        out.push_str(&format!("  block {}:\n", block.id));
        for statement in &block.statements {
            out.push_str("    ");
            out.push_str(&crate::frontend::tree::statement_text(statement, ctx));
            out.push('\n');
        }
        let tail = match &block.terminator {
            CfgTerminator::Goto(target) => format!("goto block {}", target),
            CfgTerminator::Branch(cond, a, b) => format!("branch on {} -> then block {} else block {}", crate::frontend::tree::expression_text(cond, ctx), a, b),
            CfgTerminator::Switch(subject, _, eid, targets, default) => format!(
                "switch v{} (enum #{}) -> {}{}",
                subject,
                eid,
                targets.iter().map(|(v, t)| format!("v{v}: block {t}")).collect::<Vec<_>>().join(", "),
                default.map(|d| format!(", default: block {d}")).unwrap_or_default(),
            ),
            CfgTerminator::Return(expr) => format!("return {}", crate::frontend::tree::expression_text(expr, ctx)),
            CfgTerminator::Exit(code) => format!("exit {}", crate::frontend::tree::expression_text(code, ctx)),
            // Only main can end this way: the implicit exit-0.
            CfgTerminator::Unreachable if function.is_main && !block.statements.is_empty() => "implicit exit (main)".to_owned(),
            CfgTerminator::Unreachable => "unreachable".to_owned(),
        };
        out.push_str(&format!("    {}\n", tail));
    }
}
