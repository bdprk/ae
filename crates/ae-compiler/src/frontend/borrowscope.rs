//! Borrow-scope regions along the CFG (M2.5).
//!
//! The parser registers borrows with two lifetimes: **persistent** borrows
//! (string views, token cursors, `for`-in containers, `match` subjects) last
//! until their scope or loop ends, and **call-argument borrows** last only
//! for the statement that made the call. On the graph the same distinction
//! becomes regions, one shape per borrow kind:
//!
//! - **Scope regions** (union meets, fixed point) for the borrows whose
//!   whole-statement shape the tree carries: a `for`'s container and a
//!   `match`'s subject are read by the loop's own dispatch for as long as
//!   the loop runs, so the region is the parser's scope rule expressed on
//!   the graph.
//! - **NLL regions** (backwards liveness of the view slot) for the borrows
//!   that hang off a view binding: `let v = slice_view(base, ..)` and
//!   `let cur = tokens(base, ..)`. A view is never reassigned, so its
//!   region is exactly the points where the view may still be read — the
//!   borrow dies after its last use, which is the M2.5 L1 relaxation
//!   computed here and consumed once enforcement moves onto the graph.
//!
//! This pass is verification only — the parser's registers already enforce
//! the strict (scope) rule, and every accepted program therefore keeps
//! every write outside every region under either shape. A disagreement is
//! an internal error. The NLL regions are a subset of the scope regions by
//! construction, so the check below stays silent where the parser was
//! silent; the pass runs inside every compile as a regression tripwire.
//!
//! Borrow *introductions* are recognized from the statements the tree
//! carries: `SliceView`/`Tokens` read a base; `for` loops borrow containers;
//! `match` borrows its subject. Call borrows are recognized in `Call`
//! arguments (`Argument::Borrow`) and die at the block's end.
use std::collections::HashMap;

use super::*;
use super::cfg::{build_cfg, CfgBlock, CfgFunction, CfgTerminator};
use super::movewalk::{expr_reads, statement_move_effects, statement_reads, walk_expr, walk_statement, MoveTracker};

// Adjudicate the parser's deferred write-under-view-borrow conflicts (M2.5
// L1): the parser no longer rejects a write to a string whose view may have
// died, it records the candidate and the finished function is checked here
// against the NLL regions. A write whose view may still be read later on
// some path is the real rejection — the write releases the bytes the view
// points at; a write after the view's last use is the relaxation.
//
// Every write to the base in the CFG is a deferred one (the parser defers
// all of them when only view borrows are active), so one covered write
// anywhere is reported at the first recorded conflict's site.
/// ADR-0215 (M4.1.02): a spawn with borrow captures is accepted only when
/// every path from the spawn reaches a `wait` of its handle before any exit.
/// The wait's happens-before edge is what keeps the borrowed storage safe; a
/// path that exits without joining would run the thread past the frame.
/// Blocks containing the join end every path through them, so the walk
/// explores only unjoined paths and rejects on the first exit it reaches.
pub(super) fn adjudicate_borrow_spawns(function: &Function, spawns: &[(usize, Vec<usize>, usize)]) -> Result<(), String> {
    if spawns.is_empty() { return Ok(()); }
    let graph = build_cfg(function);
    for (cid, _borrowed, _offset) in spawns {
        // The joins: blocks containing a `wait` of this spawn's handle; the
        // spawn site: the `Declare` binding a handle from THIS closure's
        // spawn expression. A borrow-capture spawn that never binds a handle
        // cannot be waited and is rejected outright.
        let mut join_blocks: Vec<usize> = Vec::new();
        let mut spawn_site: Option<(usize, usize, usize)> = None;
        for block in &graph.blocks {
            for (index, statement) in block.statements.iter().enumerate() {
                if let Statement::Declare(id, expr) = statement {
                    if let Expr::Spawn(_, callable, ..) = expr {
                        let spawned_here = match &**callable {
                            Expr::ClosureLit(lit) => lit == cid,
                            Expr::Variable(_, Type::Closure(lit), ..) => lit == cid,
                            _ => false,
                        };
                        if spawned_here {
                            spawn_site = Some((block.id, index + 1, *id));
                        }
                    }
                }
            }
        }
        let Some((spawn_block, spawn_index, handle)) = spawn_site else {
            return Err("a spawn with borrowed captures must bind its handle to a name and wait it; an unnamed handle could never be joined".to_owned());
        };
        for block in &graph.blocks {
            for statement in &block.statements {
                if wait_targets(statement).contains(&handle) {
                    join_blocks.push(block.id);
                }
            }
        }
        // Forward walk over unjoined paths.
        let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut stack: Vec<(usize, usize)> = vec![(spawn_block, spawn_index)];
        let mut escaped = false;
        while let Some((block_id, from_index)) = stack.pop() {
            let _ = from_index;
            if !visited.insert(block_id) { continue; }
            if join_blocks.contains(&block_id) { continue; }
            let Some(block) = graph.blocks.iter().find(|b| b.id == block_id) else { continue };
            let start = if block_id == spawn_block { spawn_index } else { 0 };
            if block.statements[start..].iter().any(|statement| wait_targets(statement).contains(&handle)) {
                // Joined on this path: nothing after the wait can escape.
                continue;
            }
            match &block.terminator {
                CfgTerminator::Return(_) | CfgTerminator::Exit(_) => { escaped = true; break; }
                CfgTerminator::Goto(target) => stack.push((*target, 0)),
                CfgTerminator::Branch(_, yes, no) => { stack.push((*yes, 0)); stack.push((*no, 0)); }
                CfgTerminator::Switch(_, _, targets) => {
                    for target in targets { stack.push((*target, 0)); }
                }
                // main's tail is an implicit exit; other functions' tails
                // fail loudly at runtime, which aborts the process — both
                // escape the frame.
                CfgTerminator::Unreachable => { escaped = true; break; }
            }
        }
        if escaped {
            return Err("spawn borrows from the caller but the handle is not joined before this function exits on every path; add a wait of the handle on the escaping path".to_owned());
        }
    }
    Ok(())
}

// The handle variables a statement's `wait` reads: `let v = wait(t);` and a
// wait inside a larger expression both count (ADR-0215).
fn wait_targets(statement: &Statement) -> Vec<usize> {
    let mut out = Vec::new();
    collect_waits_stmt(statement, &mut out);
    out
}

fn collect_waits_stmt(statement: &Statement, out: &mut Vec<usize>) {
    match statement {
        Statement::Return(expr, _) | Statement::Declare(_, expr) | Statement::Assign(_, expr, _) =>
            collect_waits_expr(expr, out),
        Statement::Print(Printed::Expr(expr)) => collect_waits_expr(expr, out),
        Statement::If(cond, yes, no) => {
            collect_waits_expr(cond, out);
            for s in yes { collect_waits_stmt(s, out); }
            for s in no { collect_waits_stmt(s, out); }
        }
        Statement::While(cond, body) => {
            collect_waits_expr(cond, out);
            for s in body { collect_waits_stmt(s, out); }
        }
        Statement::Match(_, _, arms) => {
            for arm in arms { for s in &arm.body { collect_waits_stmt(s, out); } }
        }
        Statement::AtomicStore(handle, value) | Statement::SharedSet(handle, value, _) => {
            collect_waits_expr(handle, out);
            collect_waits_expr(value, out);
        }
        Statement::ChanClose(handle) => {
            collect_waits_expr(handle, out);
        }
        Statement::TaskSpawn(ex, slots, callable, ..) | Statement::ScopeSpawn(ex, slots, callable, ..) | Statement::PoolSpawn(ex, slots, callable, ..) => {
            collect_waits_expr(ex, out);
            for slot in slots { collect_waits_expr(slot, out); }
            collect_waits_expr(callable, out);
        }
        Statement::TaskYield => {}
        _ => {}
    }
}

fn collect_waits_expr(expr: &Expr, out: &mut Vec<usize>) {
    if let Expr::Wait(handle, ..) = expr {
        if let Expr::Variable(id, Type::Thread(_), ..) = &**handle {
            out.push(*id);
        }
    }
    match expr {
        Expr::Spawn(slots, callable, ..) => {
            for slot in slots { collect_waits_expr(slot, out); }
            collect_waits_expr(callable, out);
        }
        Expr::Wait(handle, ..) => collect_waits_expr(handle, out),
        Expr::Binary(a, _, b, ..) => { collect_waits_expr(a, out); collect_waits_expr(b, out); }
        Expr::ReadFileBytes(path, offset, limit, ..) => {
            collect_waits_expr(path, out);
            collect_waits_expr(offset, out);
            collect_waits_expr(limit, out);
        }
        Expr::Call(_, args, _) | Expr::FnCall(_, args, _, _) => {
            for arg in args {
                match arg {
                    Argument::Value(e) | Argument::View(e) => collect_waits_expr(e, out),
                    Argument::Borrow(..) | Argument::FnPtr(_) => {}
                }
            }
        }
        _ => {}
    }
}

pub(super) fn adjudicate_deferred(function: &Function, conflicts: &[(usize, usize, String, &'static str, Option<String>)], mut_sigs: &MutSigs) -> Result<(), String> {
    if conflicts.is_empty() { return Ok(()); }
    let graph = build_cfg(function);
    let view_borrows: Vec<(usize, usize, Option<String>)> = graph.blocks.iter()
        .flat_map(|block| block.statements.iter())
        .filter_map(view_borrow_introduced)
        .collect();
    let introductions: HashMap<usize, (usize, usize)> = graph.blocks.iter()
        .flat_map(|block| {
            block.statements.iter().enumerate()
                .filter_map(|(index, statement)| view_borrow_introduced(statement).map(|(view, _, _)| (view, (block.id, index))))
        })
        .collect();
    let nll = NllRegions::compute(&graph, &view_borrows);
    let dom = dominators(&graph);
    let views_by_base = views_by_base(&view_borrows);
    for block in &graph.blocks {
        let mut suffix: Vec<std::collections::HashSet<usize>> = vec![std::collections::HashSet::new(); block.statements.len() + 1];
        for index in (0..block.statements.len()).rev() {
            let mut here = suffix[index + 1].clone();
            here.extend(statement_reads(&block.statements[index]));
            suffix[index] = here;
        }
        suffix[block.statements.len()].extend(match &block.terminator {
            CfgTerminator::Return(expr) | CfgTerminator::Exit(expr) => expr_reads(expr),
            CfgTerminator::Branch(cond, ..) => expr_reads(cond),
            CfgTerminator::Switch(subject, ..) => vec![*subject],
            _ => Vec::new(),
        });
        let live_out: std::collections::HashSet<usize> = successors(&graph, block.id)
            .iter()
            .flat_map(|succ| nll.live_in.get(succ).into_iter().flatten().copied())
            .collect();
        for (index, statement) in block.statements.iter().enumerate() {
            // Both kinds of sites re-home or release the bytes a view points
            // at: a write releases them in place; a move hands them to a new
            // owner that may die before the view does. Either way the view
            // must be dead by then — introduced before the statement
            // dominates it and read on no later path. The places decide the
            // overlap: a field write only touches that field's bytes (L2).
            let mut targets = statement_targets(statement);
            let ordered_conflict = statement_releases(statement, mut_sigs, &views_by_base);
            if std::env::var_os("AE_RELEASE_DEBUG").is_some() { eprintln!("DEBUG adjudicate b{} i{} stmt-targets={:?} ordered-rel={:?} viol={:?} view_borrows={:?} mut_sigs={:?}", block.id, index, statement_targets(statement), ordered_conflict.released, ordered_conflict.violated, view_borrows, mut_sigs); }
            if let Some(_) = ordered_conflict.violated {
                let (_, _, name, verb, _) = &conflicts[0];
                return Err(format!("cannot {verb} '{name}' while a view binding of it may still be read"));
            }
            targets.extend(ordered_conflict.released.iter().map(|base| (*base, None)));
            if targets.is_empty() { continue; }
            let mut live_at_write = suffix[index + 1].clone();
            live_at_write.extend(&live_out);
            let covered = view_borrows.iter()
                .any(|&(view, base, ref view_field)| {
                    targets.iter().any(|target| places_overlap(base, view_field, target))
                        && view_covers_write(view, block.id, index, &introductions, &dom, &live_at_write)
                });
            if covered {
                let (_, _, name, verb, _) = &conflicts[0];
                return Err(format!("cannot {verb} '{name}' while a view binding of it may still be read"));
            }
        }
        if terminator_releases_conflict(block, mut_sigs, &views_by_base, &view_borrows, &introductions, &dom, &live_out) {
            let (_, _, name, verb, _) = &conflicts[0];
            return Err(format!("cannot {verb} '{name}' while a view binding of it may still be read"));
        }
    }
    Ok(())
}

pub(super) fn verify_program_borrows(program: &Program, mut_sigs: &MutSigs) -> Result<(), String> {
    for file in &program.files {
        for function in file {
            let graph = build_cfg(function);
            BorrowRegions::run(&graph, mut_sigs)?;
        }
    }
    Ok(())
}

// The signatures whose parameters lend a struct mutably: signature id ->
// the positions of the `&mut Struct` parameters. A call through such a
// signature releases the viewed bytes of the borrowed slot at the call
// point, because the callee may write any field of it — the same reason a
// whole-variable write is a release site. The parser builds this from the
// shared signature table once per function assembly (M2.5).
pub(super) type MutSigs = HashMap<usize, Vec<usize>>;

pub(super) fn mut_borrow_positions(functions: &SharedFunctions, generics: &Generics) -> MutSigs {
    // Every id an `Expr::Call` can carry comes from one of these four
    // tables: free functions and trait impls, inherent methods, generic
    // function instances, generic method instances. A fifth source would
    // make the parser's deferral unsound, which the deferral sites guard
    // against by rejecting on sight.
    fn positions_of(params: &[Type]) -> Vec<usize> {
        params.iter().enumerate()
            .filter(|&(_, ty)| matches!(ty, Type::StructMut(_)))
            .map(|(position, _)| position)
            .collect()
    }
    fn add_instance(id: usize, params: &[Type], sigs: &mut MutSigs) {
        let positions = positions_of(params);
        if !positions.is_empty() { sigs.insert(id, positions); }
    }
    let mut sigs: MutSigs = HashMap::new();
    for signature in functions.values() { add_instance(signature.id, &signature.params, &mut sigs); }
    for method in generics.inherent.values() { add_instance(method.signature.id, &method.signature.params, &mut sigs); }
    for (id, params, _) in generics.fn_done.values() { add_instance(*id, params, &mut sigs); }
    for (id, params, _) in generics.generic_impl_done.values() { add_instance(*id, params, &mut sigs); }
    sigs
}

// The declared views grouped by their base slot: a mut call on the base
// releases every view of it (field views included — the callee may write
// any field).
fn views_by_base(view_borrows: &[(usize, usize, Option<String>)]) -> HashMap<usize, Vec<usize>> {
    let mut by_base: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(view, base, _) in view_borrows {
        by_base.entry(base).or_default().push(view);
    }
    by_base
}

// The evaluation-order release walk (M2.5): the language pins left-to-right
// operand and argument evaluation (current-language.md), and every backend
// hoists subexpressions in that order, so a view read BEFORE a mut call in
// the same statement sees the bytes intact while a read AFTER the call sees
// the released bytes — the ordered conflict this tracker reports. The
// released bases also join the statement's write targets, because the
// callee may write any field of the borrowed struct.
struct ReleaseTracker<'a> {
    mut_sigs: &'a MutSigs,
    views_by_base: &'a HashMap<usize, Vec<usize>>,
    // The view slots a release killed (a later read of one is a violation)
    // and the base slots released (they join the statement's write targets).
    released_views: std::collections::HashSet<usize>,
    released_bases: std::collections::HashSet<usize>,
    violated: Option<usize>,
}

impl MoveTracker for ReleaseTracker<'_> {
    fn read(&mut self, id: usize) -> Option<usize> {
        if self.released_views.contains(&id) { self.violated = Some(id); }
        self.violated
    }
    fn consume(&mut self, _id: usize) -> Option<usize> { self.violated }
    fn mark(&mut self, _id: usize) {}
    fn revive(&mut self, _id: usize) {}
    fn mut_release(&mut self, sig: usize, position: usize, slot: usize) {
        if self.mut_sigs.get(&sig).is_some_and(|positions| positions.contains(&position)) {
            self.release_borrowed_base(slot);
        }
    }
    fn release_borrowed_base(&mut self, slot: usize) {
        if let Some(views) = self.views_by_base.get(&slot) {
            self.released_views.extend(views.iter().copied());
            self.released_bases.insert(slot);
        }
    }
}

// The ordered walk over one statement: the released bases (mut lending call
// arguments in evaluation order) and the first view read after a release.
struct OrderedReleases {
    released: std::collections::HashSet<usize>,
    violated: Option<usize>,
}

fn statement_releases(statement: &Statement, mut_sigs: &MutSigs, views_by_base: &HashMap<usize, Vec<usize>>) -> OrderedReleases {
    let mut tracker = ReleaseTracker { mut_sigs, views_by_base, released_views: std::collections::HashSet::new(), released_bases: std::collections::HashSet::new(), violated: None };
    let violated = walk_statement(statement, &mut tracker);
    OrderedReleases { released: tracker.released_bases, violated }
}

// A mut call in the block's terminator (an `if`/`while` condition, a
// `return`'s or `exit`'s expression) releases at the block's end: nothing
// else in this block runs after it, so the views it kills are exactly the
// ones some successor may still read. True when a released view is read in
// a successor (live_out) or the ordered walk caught a same-expression read
// after the release.
fn terminator_releases_conflict(
    block: &CfgBlock,
    mut_sigs: &MutSigs,
    views_by_base: &HashMap<usize, Vec<usize>>,
    view_borrows: &[(usize, usize, Option<String>)],
    introductions: &HashMap<usize, (usize, usize)>,
    dom: &[std::collections::HashSet<usize>],
    live_out: &std::collections::HashSet<usize>,
) -> bool {
    let expr = match &block.terminator {
        CfgTerminator::Branch(cond, ..) => cond,
        CfgTerminator::Return(expr) | CfgTerminator::Exit(expr) => expr,
        _ => return false,
    };
    let mut tracker = ReleaseTracker { mut_sigs, views_by_base, released_views: std::collections::HashSet::new(), released_bases: std::collections::HashSet::new(), violated: None };
    if walk_expr(expr, &mut tracker).is_some() { return true; }
    let released = tracker.released_bases;
    if released.is_empty() { return false; }
    view_borrows.iter().any(|&(view, base, _)| {
        released.contains(&base)
            && view_covers_write(view, block.id, block.statements.len(), introductions, dom, live_out)
    })
}

// The view borrows one statement introduces: `let v = slice_view(base, ..)`
// and `let cur = tokens(base, ..)` bind a view of the base slot. The base of
// a field view (`slice_view(p.name, ..)`) is the struct itself, at the
// granularity the parser's registers use (ADR-52). The view's place is the
// base variable with the viewed field, when the base is a struct
// (`slice_view(p.name, ..)` views the place `p.name`).
fn view_borrow_introduced(statement: &Statement) -> Option<(usize, usize, Option<String>)> {
    match statement {
        Statement::Declare(view, Expr::SliceView(base, field, ..)) => Some((*view, *base, field.clone())),
        Statement::Declare(view, Expr::Tokens(base, field, ..)) => Some((*view, *base, field.clone())),
        _ => None,
    }
}

// A target place: the base slot with the written field, None for a
// whole-variable write or a move (both re-home every field's bytes).
type Place = (usize, Option<String>);

// The places one statement releases or re-homes.
fn statement_targets(statement: &Statement) -> std::collections::HashSet<Place> {
    let mut targets = std::collections::HashSet::new();
    if let Some(written) = writes_slot(statement) { targets.insert((written, None)); }
    if let Statement::FieldStore(id, fname, ..) = statement {
        // A field write releases the written field's bytes only.
        targets.remove(&(id.to_owned(), None));
        targets.insert((*id, Some(fname.clone())));
    }
    targets.extend(statement_move_effects(statement).moved.into_iter().map(|id| (id, None)));
    targets
}

// A view covers a target place when the places overlap: the same base, and
// the target is the whole variable or the very field the view reads. A
// whole-variable view (a plain string base) overlaps every write to it.
fn places_overlap(base: usize, view_field: &Option<String>, target: &Place) -> bool {
    let &(tbase, ref tfield) = target;
    tbase == base && (tfield.is_none() || *view_field == *tfield)
}

// A write conflicts with a view only when the view was introduced BEFORE the
// write on every path to it — the introduction dominates the write — and the
// view may still be read later. A view created after the write looks at the
// new bytes, never at the ones the write releases; a write is not on the
// path from an introduction to a read unless the introduction dominates it,
// because a read without a dominating introduction is a definite-assignment
// rejection the parser already makes.
fn view_covers_write(
    view: usize,
    write_block: usize,
    write_index: usize,
    introductions: &HashMap<usize, (usize, usize)>,
    dom: &[std::collections::HashSet<usize>],
    live_at_write: &std::collections::HashSet<usize>,
) -> bool {
    if !live_at_write.contains(&view) { return false; }
    match introductions.get(&view) {
        Some(&(intro_block, intro_index)) if intro_block == write_block => intro_index < write_index,
        Some(&(intro_block, _)) => dom[write_block].contains(&intro_block),
        None => false,
    }
}

struct BorrowRegions;

impl BorrowRegions {
    fn run(function: &CfgFunction, mut_sigs: &MutSigs) -> Result<(), String> {
        // The whole-statement borrows keep their union-meet regions: a
        // borrow is live on entry to a block if ANY incoming path keeps it
        // live, because a scope encloses both branch paths.
        let scope_regions = ScopeRegions::compute(function);
        // The view borrows get NLL regions: backwards liveness of the view
        // slot, which never dies by assignment (views are declare-only), so
        // a view is live exactly where some later path may still read it.
        let view_borrows: Vec<(usize, usize, Option<String>)> = function.blocks.iter()
            .flat_map(|block| block.statements.iter())
            .filter_map(view_borrow_introduced)
            .collect();
        let nll = NllRegions::compute(function, &view_borrows);
        let introductions: HashMap<usize, (usize, usize)> = function.blocks.iter()
            .flat_map(|block| {
                block.statements.iter().enumerate()
                    .filter_map(|(index, statement)| view_borrow_introduced(statement).map(|(view, _, _)| (view, (block.id, index))))
            })
            .collect();
        let dom = dominators(function);
        let views_by_base = views_by_base(&view_borrows);

        // Write-through-borrow verification: a statement that writes a slot
        // must not run inside a region covering that slot. The parser
        // enforces this with its registers (the strict scope rule); the
        // regions computed here must cover the same writes and more.
        for block in &function.blocks {
            // View slots read later in this block, by statement index:
            // suffix[i] holds the views read by statements i..end and by
            // the terminator, so a write at statement i conflicts with
            // suffix[i + 1] — the write's own right side reads first.
            let mut suffix: Vec<std::collections::HashSet<usize>> = vec![std::collections::HashSet::new(); block.statements.len() + 1];
            for index in (0..block.statements.len()).rev() {
                let mut here = suffix[index + 1].clone();
                here.extend(statement_reads(&block.statements[index]));
                suffix[index] = here;
            }
            suffix[block.statements.len()].extend(match &block.terminator {
                CfgTerminator::Return(expr) | CfgTerminator::Exit(expr) => expr_reads(expr),
                CfgTerminator::Branch(cond, ..) => expr_reads(cond),
                CfgTerminator::Switch(subject, ..) => vec![*subject],
                _ => Vec::new(),
            });

            let scope_live = &scope_regions.live_in[block.id];
            // Views live on entry to any successor: reads later on paths
            // that leave this block before it ends.
            let live_out: std::collections::HashSet<usize> = successors(function, block.id)
                .iter()
                .flat_map(|succ| nll.live_in.get(succ).into_iter().flatten().copied())
                .collect();
            for (index, statement) in block.statements.iter().enumerate() {
                let mut targets = statement_targets(statement);
                let ordered_conflict = statement_releases(statement, mut_sigs, &views_by_base);
                if ordered_conflict.violated.is_some() {
                    return Err(format!(
                        "internal: fn #{} block {} reads a view after a mut call released it; the parser's borrow registers and the ordered release walk disagree",
                        function.id, block.id
                    ));
                }
                targets.extend(ordered_conflict.released.iter().map(|base| (*base, None)));
                if targets.is_empty() { continue; }
                // The strict rule the parser enforces: no write inside a
                // scope region. The borrow's granularity is the variable
                // (ADR-52), so any touched place of the variable counts.
                let touched_vars: std::collections::HashSet<usize> = targets.iter().map(|(base, _)| *base).collect();
                for base in &touched_vars {
                    if scope_live.contains(base) {
                        return Err(format!(
                            "internal: fn #{} block {} writes v{} while its scope borrow region is live; the parser's borrow registers and the CFG regions disagree",
                            function.id, block.id, base
                        ));
                    }
                }
                // The L1/L2 rule computed here: no write or move while a
                // view of an overlapping place may still be read later on
                // some path — for a view whose introduction dominates the
                // statement. A view created after the statement looks at the
                // new bytes.
                let mut live_at_write = suffix[index + 1].clone();
                live_at_write.extend(&live_out);
                let covering: Vec<&(usize, usize, Option<String>)> = view_borrows.iter()
                    .filter(|&&(view, base, ref view_field)| {
                        targets.iter().any(|target| places_overlap(base, view_field, target))
                            && view_covers_write(view, block.id, index, &introductions, &dom, &live_at_write)
                    })
                    .collect();
                if let Some(&(view, base, _)) = covering.first() {
                    return Err(format!(
                        "internal: fn #{} block {} touches v{} while its view v{} may still be read; the parser's move rules and the NLL regions disagree",
                        function.id, block.id, base, view
                    ));
                }
            }
            if terminator_releases_conflict(block, mut_sigs, &views_by_base, &view_borrows, &introductions, &dom, &live_out) {
                return Err(format!(
                    "internal: fn #{} block {} releases a view through a terminator mut call that a successor may still read; the parser's borrow registers and the ordered release walk disagree",
                    function.id, block.id
                ));
            }
        }
        Ok(())
    }
}

// Whole-statement borrows: per-block live-on-entry sets, to a fixed point
// with union meets. Only `for` containers, token-loop bases, and match
// subjects take part — the borrows whose extent the tree's own shape fixes.
struct ScopeRegions {
    live_in: Vec<std::collections::HashSet<usize>>,
}

impl ScopeRegions {
    fn compute(function: &CfgFunction) -> ScopeRegions {
        let mut live_in: Vec<std::collections::HashSet<usize>> = vec![std::collections::HashSet::new(); function.blocks.len()];
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut in_queue = vec![false; function.blocks.len()];
        queue.push_back(0);
        in_queue[0] = true;
        while let Some(block_id) = queue.pop_front() {
            in_queue[block_id] = false;
            let mut state = live_in[block_id].clone();
            for statement in &function.blocks[block_id].statements {
                introduce_borrows(statement, &mut state);
            }
            // The terminator carries no introduction of its own: a return's
            // expression may read a borrowed base, but the region already
            // covers the block, and this walk is over introductions only.
            let exit_state = state;
            if exit_state != live_in[block_id] {
                live_in[block_id] = exit_state.clone();
                for succ in successors(function, block_id) {
                    let merged = &mut live_in[succ];
                    let before = merged.len();
                    merged.extend(exit_state.iter());
                    if merged.len() != before && !in_queue[succ] {
                        queue.push_back(succ);
                        in_queue[succ] = true;
                    }
                }
            }
        }
        ScopeRegions { live_in }
    }
}

// NLL regions for view borrows: the blocks where a view slot may still be
// read. Views are declare-only, so backwards liveness has no kills — a view
// is live on entry to a block if the block reads it or some successor does.
struct NllRegions {
    // Block id -> view slots live on ENTRY (read later on some path).
    live_in: HashMap<usize, std::collections::HashSet<usize>>,
}

impl NllRegions {
    fn compute(function: &CfgFunction, view_borrows: &[(usize, usize, Option<String>)]) -> NllRegions {
        let views: std::collections::HashSet<usize> = view_borrows.iter().map(|&(view, _, _)| view).collect();
        let mut live_in: Vec<std::collections::HashSet<usize>> = vec![std::collections::HashSet::new(); function.blocks.len()];
        let mut changed = true;
        let mut rounds = function.blocks.len() + 1;
        while changed && rounds > 0 {
            rounds -= 1;
            changed = false;
            // Reverse order converges faster along forward edges; the
            // rounds bound guards against a lowering bug, not the limit.
            for block in function.blocks.iter().rev() {
                let mut state = std::collections::HashSet::new();
                for statement in &block.statements {
                    state.extend(statement_reads(statement).into_iter().filter(|id| views.contains(id)));
                }
                match &block.terminator {
                    CfgTerminator::Return(expr) | CfgTerminator::Exit(expr) => state.extend(expr_reads(expr).into_iter().filter(|id| views.contains(id))),
                    CfgTerminator::Branch(cond, ..) => state.extend(expr_reads(cond).into_iter().filter(|id| views.contains(id))),
                    _ => {}
                }
                for succ in successors(function, block.id) {
                    state.extend(live_in[succ].iter());
                }
                if state != live_in[block.id] {
                    live_in[block.id] = state;
                    changed = true;
                }
            }
        }
        let by_id = function.blocks.iter().map(|block| (block.id, live_in[block.id].clone())).collect();
        NllRegions { live_in: by_id }
    }
}

fn successors(function: &CfgFunction, block_id: usize) -> Vec<usize> {
    match &function.blocks[block_id].terminator {
        CfgTerminator::Goto(target) => vec![*target],
        CfgTerminator::Branch(_, a, b) => vec![*a, *b],
        CfgTerminator::Switch(_, _, targets) => targets.clone(),
        CfgTerminator::Return(_) | CfgTerminator::Exit(_) | CfgTerminator::Unreachable => Vec::new(),
    }
}

// The dominators of every block: block b's dominator set is {b} plus the
// blocks every path from the entry passes through. The iterative algorithm
// over the CFG's small block counts is plenty. Unreachable blocks keep the
// full set — nothing dominates them meaningfully and nothing reaches them.
fn dominators(function: &CfgFunction) -> Vec<std::collections::HashSet<usize>> {
    let count = function.blocks.len();
    let mut preds = vec![Vec::new(); count];
    for block in function.blocks.iter() {
        for succ in successors(function, block.id) {
            if succ < count { preds[succ].push(block.id); }
        }
    }
    let full: std::collections::HashSet<usize> = (0..count).collect();
    let mut dom: Vec<std::collections::HashSet<usize>> = vec![full.clone(); count];
    if count > 0 {
        dom[0] = std::collections::HashSet::from([0]);
        let mut changed = true;
        while changed {
            changed = false;
            for id in 1..count {
                let reachable_preds: Vec<usize> = preds[id].iter().copied()
                    .filter(|pred| *pred == 0 || dom[*pred].contains(&0))
                    .collect();
                let Some(first) = reachable_preds.first() else { continue };
                let mut merged = dom[*first].clone();
                for pred in reachable_preds.iter().skip(1) {
                    merged = merged.intersection(&dom[*pred]).copied().collect();
                }
                merged.insert(id);
                if merged != dom[id] {
                    dom[id] = merged;
                    changed = true;
                }
            }
        }
    }
    dom
}

// A persistent borrow's base variable: loops borrow containers, token loops
// borrow their base, match borrows its subject. View borrows take the NLL
// path above, not the scope regions.
fn introduce_borrows(statement: &Statement, live: &mut std::collections::HashSet<usize>) {
    match statement {
        Statement::For(_, container, ..) => { live.insert(*container); }
        Statement::ForTokens(cursor, ..) => { live.insert(*cursor); }
        Statement::Match(subject, ..) => { live.insert(*subject); }
        // A payload binding (M2.3) borrows from the subject: views and
        // borrows read the enum, so its region covers the arm's block.
        Statement::BindPayload(_, subject, ..) => { live.insert(*subject); }
        _ => {}
    }
}

// The slot a statement writes, when it writes one directly: assignments,
// field stores through the owner, container pushes/stores/pops, and sorts —
// the same set the parser's registers protect.
fn writes_slot(statement: &Statement) -> Option<usize> {
    match statement {
        Statement::Assign(id, ..) => Some(*id),
        Statement::FieldStore(id, ..) => Some(*id),
        Statement::VecPush(id, ..) | Statement::StrVecPush(id, ..) | Statement::VecFPush(id, ..) => Some(*id),
        Statement::VecPop(id, ..) | Statement::StrVecPop(id, ..) | Statement::VecFPop(id, ..) => Some(*id),
        Statement::VecStore(id, ..) | Statement::StrVecStore(id, ..) | Statement::VecFStore(id, ..) => Some(*id),
        Statement::Sort(id, ..) | Statement::Reverse(id, ..) => Some(*id),
        Statement::MapPut(place, ..) => Some(place.slot()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::cfg::{CfgBlock, CfgFunction, CfgTerminator};

    fn function_of(blocks: Vec<CfgBlock>) -> CfgFunction {
        CfgFunction { id: 0, is_main: true, params: Vec::new(), returns: Type::Int, blocks }
    }

    fn block(id: usize, statements: Vec<Statement>, terminator: CfgTerminator) -> CfgBlock {
        CfgBlock { id, statements, terminator }
    }

    // A one-block function: the view is declared, the base is written while
    // the view is still readable, and the view is read afterwards.
    #[test]
    fn a_write_inside_a_view_lifetime_is_a_conflict() {
        // let v = slice_view(s, 0, 3);
        // s = "other";        <- conflict
        // print(v);
        let statements = vec![
            Statement::Declare(10, Expr::SliceView(5, None, Box::new(Expr::Number(0)), Box::new(Expr::Number(3)), 1, 1)),
            Statement::Assign(5, Expr::StrLit("other".to_owned(), 2, 8), true),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 3, 6))),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let regions = NllRegions::compute(&function, &[(10, 5, None)]);
        assert!(regions.live_in[&0].contains(&10), "the view is live on entry: {:?}", regions.live_in);
        let error = BorrowRegions::run(&function, &MutSigs::new()).unwrap_err();
        assert!(error.contains("may still be read"), "the NLL check fires: {error}");
    }

    // The same shape with the write after the view's last use: the region
    // dies at the read, so the write is outside it (L1's allowance).
    #[test]
    fn a_write_after_the_last_view_use_is_outside_the_region() {
        // let v = slice_view(s, 0, 3);
        // print(v);
        // s = "other";        <- allowed once enforcement moves onto the graph
        let statements = vec![
            Statement::Declare(10, Expr::SliceView(5, None, Box::new(Expr::Number(0)), Box::new(Expr::Number(3)), 1, 1)),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 2, 6))),
            Statement::Assign(5, Expr::StrLit("other".to_owned(), 3, 8), true),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let regions = NllRegions::compute(&function, &[(10, 5, None)]);
        // The view's last read is the print, inside this block, so the
        // entry liveness still includes it — but nothing reads the view
        // after the write, which is what the check asks.
        assert!(regions.live_in[&0].contains(&10), "the view is live on entry: {:?}", regions.live_in);
        assert!(BorrowRegions::run(&function, &MutSigs::new()).is_ok(), "no conflict after the last use");
    }

    // Scope regions keep their union meet: a for's container is borrowed for
    // the whole loop, on every path, whatever the view rules say.
    #[test]
    fn a_loop_container_stays_borrowed_across_its_scope() {
        // for t in cur { push(words, 1); }  <- words is the loop's container
        let statements = vec![
            Statement::For(20, 9, Vec::new(), 2, 1, 1),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let regions = ScopeRegions::compute(&function);
        assert!(regions.live_in[0].contains(&9), "the container is live on entry: {:?}", regions.live_in);
    }

    // The mut lending call faces (M2.5): signature #7's parameter 0 is
    // `&mut Struct`, the receiver slot is 5, and view 10 hangs off slot 5.
    fn mut_call() -> Expr {
        Expr::Call(7, vec![Argument::Borrow(5, true)], Type::Int)
    }

    fn mut_sigs() -> MutSigs {
        HashMap::from([(7usize, vec![0usize])])
    }

    fn view_declaration() -> Statement {
        Statement::Declare(10, Expr::SliceView(5, None, Box::new(Expr::Number(0)), Box::new(Expr::Number(3)), 1, 1))
    }

    // A mut call while the view may still be read later: the callee may
    // write any field of the borrowed struct, so the call releases the
    // viewed bytes — the same whole-variable rule a write takes.
    #[test]
    fn a_mut_call_inside_a_view_lifetime_is_a_conflict() {
        // let v = slice_view(s, 0, 3);
        // let r = s.mutate();   <- conflict: the callee may write any field
        // print(v);
        let statements = vec![
            view_declaration(),
            Statement::Assign(9, mut_call(), false),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 3, 6))),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let error = BorrowRegions::run(&function, &mut_sigs()).unwrap_err();
        assert!(error.contains("may still be read"), "the release fires: {error}");
    }

    // The same call after the view's last use is outside the region — the
    // M2.5 relaxation this slice brings to mut receivers.
    #[test]
    fn a_mut_call_after_the_last_view_use_is_outside_the_region() {
        // let v = slice_view(s, 0, 3);
        // print(v);
        // let r = s.mutate();   <- allowed: nothing reads the view later
        let statements = vec![
            view_declaration(),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 2, 6))),
            Statement::Assign(9, mut_call(), false),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        assert!(BorrowRegions::run(&function, &mut_sigs()).is_ok(), "no conflict after the last use");
    }

    // The ordered face: the language pins left-to-right evaluation, so a
    // view read AFTER the mut call in the same statement reads released
    // bytes — the ordered walk rejects it even though the statement-level
    // suffix rule cannot see intra-statement order.
    #[test]
    fn a_view_read_after_a_mut_call_in_one_statement_is_a_conflict() {
        // let r = s.mutate() + len(v);   <- the read follows the release
        let statements = vec![
            view_declaration(),
            Statement::Assign(9, Expr::Binary(
                Box::new(mut_call()),
                Kind::Plus,
                Box::new(Expr::Variable(10, Type::StrView, 2, 8)),
                2, 4, Type::Int,
            ), false),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let error = BorrowRegions::run(&function, &mut_sigs()).unwrap_err();
        assert!(error.contains("after a mut call"), "the ordered walk fires: {error}");
    }

    // The mirror: the read precedes the call, so it sees the bytes intact —
    // allowed, the exact allowance the ordered walk exists to keep.
    #[test]
    fn a_view_read_before_a_mut_call_in_one_statement_is_allowed() {
        // let r = len(v) + s.mutate();   <- the read precedes the release
        let statements = vec![
            view_declaration(),
            Statement::Assign(9, Expr::Binary(
                Box::new(Expr::Variable(10, Type::StrView, 2, 8)),
                Kind::Plus,
                Box::new(mut_call()),
                2, 4, Type::Int,
            ), false),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        assert!(BorrowRegions::run(&function, &mut_sigs()).is_ok(), "the read saw the bytes intact");
    }

    // A mut call in a branch condition releases at the block's end: the arm
    // that reads the view afterwards is the conflict the terminator scan
    // exists to catch — the statements-only scan would miss it.
    #[test]
    fn a_mut_call_in_a_branch_condition_releasing_to_a_reading_arm_is_a_conflict() {
        // let v = slice_view(s, 0, 3);
        // if s.mutate() == 1 { print(v); } else { }
        let statements = vec![view_declaration()];
        let then_block = block(1, vec![
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 3, 6))),
        ], CfgTerminator::Return(Expr::Number(0)));
        let else_block = block(2, Vec::new(), CfgTerminator::Return(Expr::Number(0)));
        let entry = block(0, statements, CfgTerminator::Branch(mut_call(), 1, 2));
        let function = function_of(vec![entry, then_block, else_block]);
        let error = BorrowRegions::run(&function, &mut_sigs()).unwrap_err();
        assert!(error.contains("terminator mut call"), "the terminator scan fires: {error}");
    }

    // The mut position matters: signature #7's parameter 1 is the mutable
    // one, so a borrow at position 0 (a plain read-only lend) releases
    // nothing — even though the same borrow at position 1 would.
    #[test]
    fn a_borrow_at_a_read_only_position_releases_nothing() {
        let call = Expr::Call(7, vec![Argument::Borrow(5, true), Argument::Value(Expr::Number(1))], Type::Int);
        let statements = vec![
            view_declaration(),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 2, 6))),
            Statement::Assign(9, call, false),
            Statement::Print(Printed::Expr(Expr::Variable(10, Type::StrView, 4, 8))),
        ];
        let function = function_of(vec![block(0, statements, CfgTerminator::Unreachable)]);
        let sigs: MutSigs = HashMap::from([(7usize, vec![1usize])]);
        assert!(BorrowRegions::run(&function, &sigs).is_ok(), "position 0 is not the mutable one");
    }
}
