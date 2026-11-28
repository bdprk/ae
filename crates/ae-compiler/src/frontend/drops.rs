//! Drop elaboration over the MIR block graph (M2.4).
//!
//! The parser attaches a release list to every point where storage leaves
//! scope: the end of a block, `break`/`continue`, and the releases a `return`
//! or `exit` carries. Those lists are written before the graph exists, and
//! they are conservative in both directions:
//!
//! - they list a slot a statement has already **moved out** of (the move
//!   zeroed the slot, so the release frees nothing but still branches and
//!   stores), and
//! - they list a slot a path can reach **before it was ever written** (a
//!   release of an uninitialized slot would free a garbage pointer).
//!
//! This pass turns each list into what the graph says it must be. It computes,
//! per block and per owned local, whether the slot holds a value that still
//! needs releasing on **every** incoming path (`Live`), on **none** of them
//! (`Uninit` for a slot no path wrote, `Dropped` for a slot every path already
//! moved or released), or on **some** of them (`Mixed`). A `Live` entry stays,
//! `Uninit`/`Dropped` entries are removed, and a `Mixed` entry stays too: a
//! branch-local move of an outer binding reaches such a release, and the
//! conservative emission is exactly what the tree backend has always shipped —
//! the slot is zeroed when a move takes the value, so the idempotent release
//! frees on the path that still holds the value and no-ops on every other.
//!
//! The meet is exact: `Live ⊔ Live = Live`, `Uninit ⊔ Uninit = Uninit`,
//! `Dropped ⊔ Dropped = Dropped`, and any other pair joins to `Mixed`. Move
//! events come from `movewalk::statement_move_effects`, the same reading of
//! "what consumes a binding" the move verifier uses, so the two analyses
//! cannot disagree about what a statement moves.
use std::collections::HashMap;

use super::*;
use super::cfg::CfgTerminator;
use super::mir::{MirBinding, MirFunction, MirOperand, MirPlace, MirRvalue};
use super::movewalk::statement_move_effects;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DropState {
    // No path wrote this slot: releasing it would free a garbage pointer.
    Uninit,
    // Every path reaching here holds a value that still needs releasing.
    Live,
    // Every path reaching here already moved or released the value.
    Dropped,
    // Some paths hold a value, others do not: a runtime drop flag would say
    // which, but the zeroed slot already does — releasing is safe on every
    // path, so the release is kept rather than proven.
    Mixed,
}

fn join(a: DropState, b: DropState) -> DropState {
    if a == b { a } else { DropState::Mixed }
}

type State = HashMap<usize, DropState>;

// What the pass changed, so the compiler can report it and tests can assert
// that a program's releases were actually reconsidered rather than passed
// through.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DropStats {
    // Releases kept: a live value on every path.
    pub(super) kept: usize,
    // Releases kept although the state could not prove anything: the paths
    // disagree, so the idempotent release decides at runtime by finding the
    // slot zeroed or not.
    pub(super) kept_mixed: usize,
    // Releases removed because every path already moved the value.
    pub(super) elided_dropped: usize,
    // Releases removed because no path ever wrote the slot.
    pub(super) elided_uninit: usize,
}

pub(super) fn elaborate(function: &mut MirFunction) -> DropStats {
    let rounds = function.blocks.len().saturating_mul(4).saturating_add(8);
    elaborate_with_round_limit(function, rounds)
}

fn elaborate_with_round_limit(function: &mut MirFunction, rounds: usize) -> DropStats {
    let Some(states) = entry_states(function, rounds) else {
        // An incomplete dataflow result cannot justify removing any release.
        return DropStats::default();
    };
    let mut stats = DropStats::default();
    for block in &mut function.blocks {
        let Some(mut state) = states.get(&block.id).cloned() else { continue };
        if std::env::var_os("AE_DROP_DEBUG").is_some() { eprintln!("DEBUG fn#{} block{} entry={state:?} statements={:?}", function.id, block.id, block.statements.iter().map(|s| format!("{s:?}").chars().take(120).collect::<String>()).collect::<Vec<_>>()); }
        let statements = std::mem::take(&mut block.statements);
        let mut kept = Vec::with_capacity(statements.len());
        // Each release list decomposed into a `releases(...)` binding keeps
        // its place in the block's binding list; the two views must show the
        // same releases after the pass, or emit-mir would print releases the
        // shipped C does not run (bindings are the printer's view, statements
        // the C backend's).
        let mut consumed = 0usize;
        for statement in statements {
            match statement {
                Statement::Frees(list) => {
                    let original: Vec<usize> = list.iter().map(|(id, _)| *id).collect();
                    let kept_list = rewrite_list(list, &mut state, &mut stats, function.id, block.id);
                    sync_releases_binding(&mut block.bindings, &original, &kept_list, &mut consumed);
                    if !kept_list.is_empty() { kept.push(Statement::Frees(kept_list)); }
                }
                statement => {
                    if let Some(statement) = rewrite(statement, &mut state, &mut stats, function.id, block.id) {
                        kept.push(statement);
                    }
                }
            }
        }
        block.statements = kept;
    }
    stats
}

// Rewrite the decomposition of one `Frees` statement: the binding whose
// operands match the original list in order is the one this statement
// lowered into (each Frees contributes exactly one binding, in statement
// order, so first-unconsumed-match in order is the right one). The kept
// slots become the binding's new operands; an empty kept list removes the
// binding the way the statement itself is removed.
fn sync_releases_binding(bindings: &mut Vec<MirBinding>, original: &[usize], kept: &[(usize, Type)], consumed: &mut usize) {
    let mut index = *consumed;
    while index < bindings.len() {
        if let MirRvalue::Intrinsic(name, ops, _) = &bindings[index].rvalue
            && *name == "releases" {
                let ids: Vec<usize> = ops.iter().filter_map(|op| match op {
                    MirOperand::Place(MirPlace::Local(id, _)) => Some(*id),
                    _ => None,
                }).collect();
                if ids == original {
                    if kept.is_empty() {
                        bindings.remove(index);
                    } else {
                        let new_ops: Vec<MirOperand> = kept.iter().map(|&(id, ty)| MirOperand::Place(MirPlace::Local(id, ty))).collect();
                        bindings[index].rvalue = MirRvalue::Intrinsic("releases", new_ops, Type::Int(IntTy::I64));
                    }
                    *consumed = index;
                    return;
                }
            }
        index += 1;
    }
}

// Per-block state on entry, to a fixed point. `Uninit` is the default for
// every slot a state does not name, so the entry state names only the owned
// parameters (live on entry).
fn entry_states(function: &MirFunction, rounds: usize) -> Option<HashMap<usize, State>> {
    let Some(entry) = function.blocks.first().map(|block| block.id) else { return Some(HashMap::new()) };
    let mut initial = State::new();
    for &(id, ty) in &function.params {
        if ty.owned() { initial.insert(id, DropState::Live); }
    }
    let mut in_states: HashMap<usize, State> = HashMap::new();
    let mut out_states: HashMap<usize, State> = HashMap::new();
    // Each round recomputes every block from its predecessors' current exit
    // states. States only move up the lattice (`Mixed` is the top), so the
    // iteration terminates; the round bound is a guard against a lowering bug
    // introducing a cycle the lattice cannot absorb, not the real limit.
    for _ in 0..rounds {
        let mut changed = false;
        for block in &function.blocks {
            let incoming = if block.id == entry {
                initial.clone()
            } else if let Some(state) = merge_predecessors(function, block.id, &out_states) {
                state
            } else {
                continue;
            };
            let mut exit = incoming.clone();
            transfer_statements(&block.statements, &mut exit);
            if in_states.get(&block.id) != Some(&incoming) {
                in_states.insert(block.id, incoming);
                changed = true;
            }
            if out_states.get(&block.id) != Some(&exit) {
                out_states.insert(block.id, exit);
                changed = true;
            }
        }
        if !changed { return Some(in_states); }
    }
    None
}

fn merge_predecessors(function: &MirFunction, block_id: usize, out_states: &HashMap<usize, State>) -> Option<State> {
    let mut merged: Option<State> = None;
    for block in &function.blocks {
        if !terminator_targets(&block.terminator).contains(&block_id) { continue; }
        let Some(exit) = out_states.get(&block.id).cloned() else { continue };
        merged = Some(match merged {
            None => exit,
            Some(joined) => join_states(&joined, &exit),
        });
    }
    merged
}

fn join_states(a: &State, b: &State) -> State {
    let mut joined = State::new();
    for &id in a.keys().chain(b.keys()) {
        let left = a.get(&id).copied().unwrap_or(DropState::Uninit);
        let right = b.get(&id).copied().unwrap_or(DropState::Uninit);
        joined.insert(id, join(left, right));
    }
    joined
}

fn terminator_targets(terminator: &CfgTerminator) -> Vec<usize> {
    match terminator {
        CfgTerminator::Goto(target) => vec![*target],
        CfgTerminator::Branch(_, yes, no) => vec![*yes, *no],
        CfgTerminator::Switch(_, _, _, targets, default) => {
            let mut out: Vec<usize> = targets.iter().map(|(_, t)| *t).collect();
            if let Some(d) = default {
                out.push(*d);
            }
            out
        }
        CfgTerminator::Return(_) | CfgTerminator::Exit(_) | CfgTerminator::Unreachable => Vec::new(),
    }
}

// The state a statement leaves behind, without rewriting it. Releases drop
// every listed slot; the moves and re-initializations come from the shared
// move walk, applied in the order a statement performs them.
//
// Terminators need no transfer of their own: `return`/`exit` end the block, so
// the only paths that read a post-terminator state are `goto`/`branch`/`switch`
// edges, and their conditions are emitted without moving anything.
fn transfer_statements(statements: &[Statement], state: &mut State) {
    for statement in statements {
        transfer_statement(statement, state);
    }
}

fn transfer_statement(statement: &Statement, state: &mut State) {
    match statement {
        Statement::Frees(list) => drop_list(list, state),
        // A return/exit carries its releases on the statement in the tree; the
        // CFG lowering moves them into the block, so this arm covers the tree
        // shape only.
        Statement::Return(_, list) | Statement::Exit(_, list) => drop_list(list, state),
        // Control statements never survive into a MIR block (the lowering
        // turns them into blocks and edges). The arms stay total: a nested
        // list is joined into the state the way its own blocks would be.
        Statement::If(_, yes, no) => {
            let mut then_state = state.clone();
            transfer_statements(yes, &mut then_state);
            let mut else_state = state.clone();
            transfer_statements(no, &mut else_state);
            *state = join_states(&then_state, &else_state);
        }
        Statement::While(_, body) | Statement::For(_, _, body, ..) | Statement::ForTokens(_, _, body, ..) => {
            let mut body_state = state.clone();
            transfer_statements(body, &mut body_state);
            let entry_state = state.clone();
            *state = join_states(&entry_state, &body_state);
        }
        Statement::Match(_, _, arms) => {
            let mut merged = state.clone();
            for arm in arms {
                let mut arm_state = state.clone();
                transfer_statements(&arm.body, &mut arm_state);
                merged = join_states(&merged, &arm_state);
            }
            *state = merged;
        }
        other => apply_effects(other, state),
    }
}

// A statement's own moves and re-initializations. The right side evaluates
// before the slot is written, so the move events apply first.
fn apply_effects(statement: &Statement, state: &mut State) {
    let effects = statement_move_effects(statement);
    for id in effects.moved {
        state.insert(id, DropState::Dropped);
    }
    for id in effects.revived {
        state.insert(id, DropState::Live);
    }
}

fn drop_list(list: &[(usize, Type)], state: &mut State) {
    for &(id, _) in list {
        state.insert(id, DropState::Dropped);
    }
}

// Rewrite one statement's release lists. `Ok(None)` means the statement does
// nothing any more and is removed, so a block can become empty and eligible
// for edge threading again.
fn rewrite(statement: Statement, state: &mut State, stats: &mut DropStats, function: usize, block: usize) -> Option<Statement> {
    match statement {
        Statement::Frees(list) => {
            let kept = rewrite_list(list, state, stats, function, block);
            if kept.is_empty() { return None; }
            Some(Statement::Frees(kept))
        }
        Statement::Return(expr, list) => Some(Statement::Return(expr, rewrite_list(list, state, stats, function, block))),
        Statement::Exit(code, list) => Some(Statement::Exit(code, rewrite_list(list, state, stats, function, block))),
        // A `?`'s releases ride the error path, which exits the function, so
        // the state to read them with is the one after the value evaluated —
        // its moves first, then the releases. They say nothing about the
        // continuing path: the Ok branch never runs them, so the continuing
        // state keeps the moves' result and the releases are rewritten
        // against a copy of the state.
        prop @ Statement::Propagate(..) => {
            let effects = statement_move_effects(&prop);
            for id in effects.moved {
                state.insert(id, DropState::Dropped);
            }
            let Statement::Propagate(expr, ty, fields, binding, releases) = prop else { unreachable!("matched above") };
            let mut error_state = state.clone();
            let kept = rewrite_list(releases, &mut error_state, stats, function, block);
            for id in effects.revived {
                state.insert(id, DropState::Live);
            }
            // The statement itself always survives: it still unwraps and
            // returns on the error path; only its release list thins.
            Some(Statement::Propagate(expr, ty, fields, binding, kept))
        }
        other => {
            apply_effects(&other, state);
            Some(other)
        }
    }
}

fn rewrite_list(list: Vec<(usize, Type)>, state: &mut State, stats: &mut DropStats, function: usize, block: usize) -> Vec<(usize, Type)> {
    let mut kept = Vec::with_capacity(list.len());
    for (id, ty) in list {
        match state.get(&id).copied().unwrap_or(DropState::Uninit) {
            DropState::Live => {
                stats.kept += 1;
                state.insert(id, DropState::Dropped);
                kept.push((id, ty));
            }
            DropState::Dropped => stats.elided_dropped += 1,
            DropState::Uninit => {
                if std::env::var_os("AE_DROP_DEBUG").is_some() { eprintln!("DEBUG elide-uninit v{id} in fn#{function} block{block} state={state:?} statements-seen"); }
                stats.elided_uninit += 1
            }
            // The paths disagree, so no proof decides the release. Keep it:
            // a move zeroes the slot it takes from, so the release frees on
            // the paths that still hold the value and no-ops on every other —
            // the exact C the tree backend emits for the same program. After
            // an idempotent release the slot is dropped on every path.
            DropState::Mixed => {
                stats.kept_mixed += 1;
                state.insert(id, DropState::Dropped);
                kept.push((id, ty));
            }
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unvisited_backedge_does_not_poison_live_loop_state() {
        use super::super::mir::MirBlock;
        let function = MirFunction {
            id: 0, is_main: false, params: vec![(7, Type::String)], returns: Type::Int(IntTy::I64),
            blocks: vec![
                MirBlock { id: 0, statements: vec![], bindings: vec![], terminator: CfgTerminator::Goto(1) },
                MirBlock { id: 1, statements: vec![], bindings: vec![], terminator: CfgTerminator::Branch(Expr::BoolLit(true), 1, 2) },
                MirBlock { id: 2, statements: vec![], bindings: vec![], terminator: CfgTerminator::Unreachable },
            ],
        };
        let states = entry_states(&function, 20).expect("dataflow must converge");
        assert_eq!(states[&1].get(&7), Some(&DropState::Live));
        assert_eq!(states[&2].get(&7), Some(&DropState::Live));
    }

    #[test]
    fn later_predecessor_is_joined_and_unreachable_block_is_preserved() {
        use super::super::mir::MirBlock;
        let mut function = MirFunction {
            id: 0, is_main: false, params: vec![(7, Type::String)], returns: Type::Int(IntTy::I64),
            blocks: vec![
                MirBlock { id: 0, statements: vec![], bindings: vec![], terminator: CfgTerminator::Branch(Expr::BoolLit(true), 1, 2) },
                MirBlock { id: 1, statements: vec![], bindings: vec![], terminator: CfgTerminator::Goto(3) },
                MirBlock { id: 3, statements: vec![], bindings: vec![], terminator: CfgTerminator::Unreachable },
                MirBlock { id: 2, statements: vec![Statement::Frees(vec![(7, Type::String)])], bindings: vec![], terminator: CfgTerminator::Goto(3) },
                MirBlock { id: 4, statements: vec![Statement::Frees(vec![(8, Type::String)])], bindings: vec![], terminator: CfgTerminator::Unreachable },
            ],
        };
        let states = entry_states(&function, 30).expect("dataflow must converge");
        assert_eq!(states[&3].get(&7), Some(&DropState::Mixed));
        assert!(!states.contains_key(&4));
        elaborate(&mut function);
        assert!(matches!(&function.blocks[4].statements[..], [Statement::Frees(items)] if items[0].0 == 8));
    }

    #[test]
    fn incomplete_analysis_does_not_rewrite_releases() {
        let mut function = MirFunction {
            id: 0, is_main: false, params: vec![], returns: Type::Int(IntTy::I64),
            blocks: vec![super::super::mir::MirBlock {
                id: 0,
                statements: vec![Statement::Frees(vec![(7, Type::String)])],
                bindings: vec![],
                terminator: CfgTerminator::Unreachable,
            }],
        };
        let stats = elaborate_with_round_limit(&mut function, 0);
        assert_eq!(stats.elided_uninit, 0);
        assert!(matches!(&function.blocks[0].statements[..], [Statement::Frees(items)] if items.len() == 1 && items[0].0 == 7));
    }

    #[test]
    fn join_treats_missing_slots_as_uninitialized_on_either_side() {
        let live = State::from([(7, DropState::Live)]);
        let absent = State::new();
        for (left, right) in [(&live, &absent), (&absent, &live)] {
            assert_eq!(join_states(left, right).get(&7), Some(&DropState::Mixed));
        }
        let dropped = State::from([(7, DropState::Dropped)]);
        for (left, right) in [(&dropped, &absent), (&absent, &dropped)] {
            assert_eq!(join_states(left, right).get(&7), Some(&DropState::Mixed));
        }
    }
}
