//! Field-granular move/initialization analysis and NLL borrow checker on pure MIR (P3.3).
//!
//! Operates purely on [`Body`] without requiring `ae-compiler` or `TypeInterner`:
//! - Field-granular initialization & move dataflow across CFG merges and loops.
//! - NLL loan generation at `Rvalue::Ref`, flow-sensitive loan propagation,
//!   backward local liveness, conflict detection, and escaping-reference checks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use ae_ty::TyKind;

use crate::analysis::Cfg;
use crate::ir::{
    AssertMessage, BasicBlockId, Body, BorrowKind, LocalId, LocalKind, Operand, Place,
    ProjectionElem, Rvalue, StatementKind, TerminatorKind,
};

/// A program point within a MIR [`Body`].
///
/// `statement_index == body.basic_blocks[block.index()].statements.len()` denotes
/// the block's terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Location {
    pub block: BasicBlockId,
    pub statement_index: usize,
}

impl Location {
    pub const fn new(block: BasicBlockId, statement_index: usize) -> Self {
        Self {
            block,
            statement_index,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}[{}]", self.block.0, self.statement_index)
    }
}

/// A borrow loan created by [`Rvalue::Ref`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Loan {
    pub id: usize,
    pub kind: BorrowKind,
    pub place: Place,
    pub issued_at: Location,
}

/// Classification of an access that conflicted with an active loan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessKind {
    Read,
    Write,
    Move,
    Borrow(BorrowKind),
}

impl fmt::Display for AccessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => f.write_str("read"),
            Self::Write => f.write_str("write"),
            Self::Move => f.write_str("move"),
            Self::Borrow(BorrowKind::Shared) => f.write_str("shared borrow"),
            Self::Borrow(BorrowKind::Mut) => f.write_str("mutable borrow"),
        }
    }
}

/// Specific borrow/initialization violation detected by [`check_body`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BorrowErrorKind {
    /// Reading, moving, or borrowing a place that was never initialized.
    UseOfUninitialized { place: Place },
    /// Reading, moving, or borrowing a place that was already moved.
    UseOfMoved { place: Place },
    /// Reading, moving, or borrowing a place that may be uninitialized or moved on some path.
    UseOfMaybeUninitialized { place: Place },
    /// Accessing a place in a way that conflicts with a live loan.
    BorrowConflict {
        access: AccessKind,
        place: Place,
        loan: Loan,
    },
    /// Returning a reference that originates from local storage rather than an input borrow.
    EscapingReference { local: LocalId, loan: Loan },
}

/// Borrow-checker error paired with the [`Location`] where it occurred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BorrowError {
    pub location: Location,
    pub kind: BorrowErrorKind,
}

impl fmt::Display for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BorrowErrorKind::UseOfUninitialized { place } => {
                write!(f, "{}: use of uninitialized place `{place}`", self.location)
            }
            BorrowErrorKind::UseOfMoved { place } => {
                write!(f, "{}: use of moved place `{place}`", self.location)
            }
            BorrowErrorKind::UseOfMaybeUninitialized { place } => {
                write!(
                    f,
                    "{}: use of possibly uninitialized or moved place `{place}`",
                    self.location
                )
            }
            BorrowErrorKind::BorrowConflict {
                access,
                place,
                loan,
            } => {
                let loan_kind = match loan.kind {
                    BorrowKind::Shared => "shared",
                    BorrowKind::Mut => "mutable",
                };
                write!(
                    f,
                    "{}: cannot {access} `{place}` while `{}` is {loan_kind}ly borrowed (loan #{} issued at {})",
                    self.location, loan.place, loan.id, loan.issued_at
                )
            }
            BorrowErrorKind::EscapingReference { local, loan } => {
                write!(
                    f,
                    "{}: returning `_{}` escapes local borrow `{}` (loan #{} issued at {})",
                    self.location, local.0, loan.place, loan.id, loan.issued_at
                )
            }
        }
    }
}

/// Checks initialization, field-granular moves, NLL borrow conflicts, and escaping references
/// on a pure MIR [`Body`].
pub fn check_body(body: &Body) -> Result<(), Vec<BorrowError>> {
    if body.basic_blocks.is_empty() || body.locals.is_empty() {
        return Ok(());
    }

    let cfg = Cfg::compute(body);
    let init_entry = compute_field_init_dataflow(body, &cfg);
    let loans = collect_loans(body, &cfg);
    let reaching_entry = compute_reaching_loans(body, &cfg, &loans);
    let live_out = compute_live_locals(body, &cfg);

    let mut errors = Vec::new();

    for &bb_id in &cfg.rpo {
        if !cfg.is_reachable(bb_id) {
            continue;
        }
        let idx = bb_id.index();
        let Some(bb) = body.basic_blocks.get(idx) else {
            continue;
        };

        let mut init_state = init_entry
            .get(idx)
            .cloned()
            .unwrap_or_else(|| vec![LocalInitTree::uniform(PathInitState::Uninit); body.locals.len()]);
        let mut reaching_state = reaching_entry
            .get(idx)
            .cloned()
            .unwrap_or_else(|| vec![BTreeSet::new(); body.locals.len()]);

        let stmt_live_before = compute_block_stmt_liveness(
            bb,
            body.locals.len(),
            live_out.get(idx).cloned().unwrap_or_default(),
        );

        for (stmt_idx, stmt) in bb.statements.iter().enumerate() {
            let loc = Location::new(bb_id, stmt_idx);
            let live_before = stmt_live_before.get(stmt_idx).cloned().unwrap_or_default();

            check_and_step_statement(
                &stmt.kind,
                loc,
                &loans,
                &live_before,
                &mut init_state,
                &mut reaching_state,
                &mut errors,
            );
        }

        if let Some(term) = &bb.terminator {
            let term_loc = Location::new(bb_id, bb.statements.len());
            let live_before = stmt_live_before
                .get(bb.statements.len())
                .cloned()
                .unwrap_or_default();

            check_and_step_terminator(
                body,
                &term.kind,
                term_loc,
                &loans,
                &live_before,
                &mut init_state,
                &mut reaching_state,
                &mut errors,
            );
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ============================================================================
// Field-granular initialization & move dataflow
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PathInitState {
    Uninit,
    Init,
    Moved,
    MaybeInit,
}

impl PathInitState {
    fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::MaybeInit
        }
    }
}

/// Hierarchical initialization tree for a single local variable.
///
/// `default_state` applies to all fields not explicitly overridden in `fields`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalInitTree {
    default_state: PathInitState,
    fields: BTreeMap<usize, LocalInitTree>,
}

impl LocalInitTree {
    fn uniform(state: PathInitState) -> Self {
        Self {
            default_state: state,
            fields: BTreeMap::new(),
        }
    }

    fn join(&self, other: &Self) -> Self {
        let default_state = self.default_state.join(other.default_state);
        let mut fields = BTreeMap::new();

        let keys: BTreeSet<usize> = self
            .fields
            .keys()
            .chain(other.fields.keys())
            .copied()
            .collect();

        for key in keys {
            let a = self
                .fields
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Self::uniform(self.default_state));
            let b = other
                .fields
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Self::uniform(other.default_state));
            let joined = a.join(&b);
            if !(joined.fields.is_empty() && joined.default_state == default_state) {
                fields.insert(key, joined);
            }
        }

        Self {
            default_state,
            fields,
        }
    }

    fn effective_state(&self) -> PathInitState {
        let mut acc = self.default_state;
        for child in self.fields.values() {
            acc = acc.join(child.effective_state());
        }
        acc
    }

    fn get_path(&self, path: &[usize]) -> PathInitState {
        let mut cur = self;
        for &field in path {
            if let Some(child) = cur.fields.get(&field) {
                cur = child;
            } else {
                return cur.default_state;
            }
        }
        cur.effective_state()
    }

    fn set_path(&mut self, path: &[usize], state: PathInitState) {
        if path.is_empty() {
            self.default_state = state;
            self.fields.clear();
            return;
        }
        let first = path[0];
        let rest = &path[1..];
        let default = self.default_state;
        let child = self
            .fields
            .entry(first)
            .or_insert_with(|| Self::uniform(default));
        child.set_path(rest, state);
        if child.fields.is_empty() && child.default_state == self.default_state {
            self.fields.remove(&first);
        }
    }
}

/// Extracted owned field path before any `Deref` or `Index`.
struct PlacePathInfo {
    fields: Vec<usize>,
    has_deref: bool,
    has_index: bool,
}

fn extract_place_path(place: &Place) -> PlacePathInfo {
    let mut fields = Vec::new();
    let mut has_deref = false;
    let mut has_index = false;

    for elem in &place.projection {
        match elem {
            ProjectionElem::Field(idx) => {
                if !has_deref && !has_index {
                    fields.push(*idx);
                }
            }
            ProjectionElem::Downcast(_) => {}
            ProjectionElem::Deref => {
                has_deref = true;
                break;
            }
            ProjectionElem::Index(_) => {
                has_index = true;
                break;
            }
        }
    }

    PlacePathInfo {
        fields,
        has_deref,
        has_index,
    }
}

fn compute_field_init_dataflow(body: &Body, cfg: &Cfg) -> Vec<Vec<LocalInitTree>> {
    let num_blocks = body.basic_blocks.len();
    let num_locals = body.locals.len();

    let mut entry_states =
        vec![vec![LocalInitTree::uniform(PathInitState::Uninit); num_locals]; num_blocks];
    let mut exit_states =
        vec![vec![LocalInitTree::uniform(PathInitState::Uninit); num_locals]; num_blocks];

    if num_blocks == 0 {
        return entry_states;
    }

    let mut init_entry = vec![LocalInitTree::uniform(PathInitState::Uninit); num_locals];
    let arg_end = body.arg_count.min(num_locals.saturating_sub(1));
    for slot in init_entry.iter_mut().take(arg_end + 1).skip(1) {
        *slot = LocalInitTree::uniform(PathInitState::Init);
    }
    entry_states[0] = init_entry;

    let mut initialized_block = vec![false; num_blocks];
    let mut worklist: VecDeque<BasicBlockId> = cfg.rpo.iter().copied().collect();
    let mut in_queue = vec![false; num_blocks];
    for &bb in &cfg.rpo {
        in_queue[bb.index()] = true;
    }

    while let Some(bb_id) = worklist.pop_front() {
        let idx = bb_id.index();
        in_queue[idx] = false;
        if !cfg.is_reachable(bb_id) {
            continue;
        }

        let new_entry = if idx == 0 && cfg.predecessors(bb_id).is_empty() {
            entry_states[0].clone()
        } else {
            let mut joined: Option<Vec<LocalInitTree>> = if idx == 0 {
                Some(entry_states[0].clone())
            } else {
                None
            };
            for &pred in cfg.predecessors(bb_id) {
                if !initialized_block[pred.index()] {
                    continue;
                }
                let pred_exit = &exit_states[pred.index()];
                match &mut joined {
                    None => joined = Some(pred_exit.clone()),
                    Some(acc) => {
                        for (a, b) in acc.iter_mut().zip(pred_exit.iter()) {
                            *a = a.join(b);
                        }
                    }
                }
            }
            joined.unwrap_or_else(|| {
                vec![LocalInitTree::uniform(PathInitState::Uninit); num_locals]
            })
        };

        entry_states[idx] = new_entry.clone();
        let mut state = new_entry;
        if let Some(bb) = body.basic_blocks.get(idx) {
            for stmt in &bb.statements {
                transfer_init_stmt(&stmt.kind, &mut state);
            }
            if let Some(term) = &bb.terminator {
                transfer_init_term(&term.kind, &mut state);
            }
        }

        let changed = !initialized_block[idx] || exit_states[idx] != state;
        initialized_block[idx] = true;
        if changed {
            exit_states[idx] = state;
            for &succ in cfg.successors(bb_id) {
                if !in_queue[succ.index()] {
                    in_queue[succ.index()] = true;
                    worklist.push_back(succ);
                }
            }
        }
    }

    entry_states
}

fn apply_move_place(place: &Place, state: &mut [LocalInitTree]) {
    let info = extract_place_path(place);
    if info.has_deref {
        return;
    }
    if let Some(tree) = state.get_mut(place.local.index()) {
        if info.has_index {
            tree.set_path(&info.fields, PathInitState::MaybeInit);
        } else {
            tree.set_path(&info.fields, PathInitState::Moved);
        }
    }
}

fn apply_write_place(place: &Place, state: &mut [LocalInitTree]) {
    let info = extract_place_path(place);
    if info.has_deref || info.has_index {
        return;
    }
    if let Some(tree) = state.get_mut(place.local.index()) {
        tree.set_path(&info.fields, PathInitState::Init);
    }
}

fn apply_drop_place(place: &Place, state: &mut [LocalInitTree]) {
    let info = extract_place_path(place);
    if info.has_deref {
        return;
    }
    if let Some(tree) = state.get_mut(place.local.index()) {
        if info.has_index {
            tree.set_path(&info.fields, PathInitState::MaybeInit);
        } else {
            tree.set_path(&info.fields, PathInitState::Moved);
        }
    }
}

fn apply_rvalue_moves(rvalue: &Rvalue, state: &mut [LocalInitTree]) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            if let Operand::Move(p) = op {
                apply_move_place(p, state);
            }
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            if let Operand::Move(p) = lhs {
                apply_move_place(p, state);
            }
            if let Operand::Move(p) = rhs {
                apply_move_place(p, state);
            }
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                if let Operand::Move(p) = op {
                    apply_move_place(p, state);
                }
            }
        }
        Rvalue::Ref { .. } | Rvalue::Len(_) | Rvalue::Discriminant(_) => {}
    }
}

fn transfer_init_stmt(kind: &StatementKind, state: &mut [LocalInitTree]) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            apply_rvalue_moves(rvalue, state);
            apply_write_place(place, state);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            if let Some(tree) = state.get_mut(local.index()) {
                *tree = LocalInitTree::uniform(PathInitState::Uninit);
            }
        }
        StatementKind::SetDiscriminant { place, .. } => {
            let info = extract_place_path(place);
            if !info.has_deref
                && !info.has_index
                && let Some(tree) = state.get_mut(place.local.index())
                && tree.get_path(&info.fields) == PathInitState::Uninit
            {
                tree.set_path(&info.fields, PathInitState::MaybeInit);
            }
        }
        StatementKind::Drop(place) => apply_drop_place(place, state),
        StatementKind::Nop => {}
    }
}

fn transfer_init_term(kind: &TerminatorKind, state: &mut [LocalInitTree]) {
    match kind {
        TerminatorKind::SwitchInt { discr, .. } => {
            if let Operand::Move(p) = discr {
                apply_move_place(p, state);
            }
        }
        TerminatorKind::Call {
            func,
            args,
            destination,
            target,
        } => {
            if let Operand::Move(p) = func {
                apply_move_place(p, state);
            }
            for arg in args {
                if let Operand::Move(p) = arg {
                    apply_move_place(p, state);
                }
            }
            if target.is_some() {
                apply_write_place(destination, state);
            }
        }
        TerminatorKind::Drop { place, .. } => apply_drop_place(place, state),
        TerminatorKind::Assert { cond, .. } => {
            if let Operand::Move(p) = cond {
                apply_move_place(p, state);
            }
        }
        TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => {}
    }
}

// ============================================================================
// NLL loans, flow-sensitive reaching loans, and backward local liveness
// ============================================================================

#[derive(Clone, Debug)]
struct LoanTable {
    loans: Vec<Loan>,
    /// Map from `Location` (of `Rvalue::Ref`) to `Loan::id`.
    by_location: BTreeMap<Location, usize>,
    /// True if the loan originates from local storage in this body (not an input borrow).
    from_local_storage: Vec<bool>,
}

fn collect_loans(body: &Body, cfg: &Cfg) -> LoanTable {
    let mut loans = Vec::new();
    let mut by_location = BTreeMap::new();

    for (bb_idx, bb) in body.basic_blocks.iter().enumerate() {
        let bb_id = BasicBlockId(bb_idx as u32);
        if !cfg.is_reachable(bb_id) {
            continue;
        }
        for (stmt_idx, stmt) in bb.statements.iter().enumerate() {
            if let StatementKind::Assign(_, Rvalue::Ref { borrow_kind, place }) = &stmt.kind {
                let id = loans.len();
                let loc = Location::new(bb_id, stmt_idx);
                loans.push(Loan {
                    id,
                    kind: *borrow_kind,
                    place: place.clone(),
                    issued_at: loc,
                });
                by_location.insert(loc, id);
            }
        }
    }

    let from_local_storage = vec![true; loans.len()];
    let mut table = LoanTable {
        loans,
        by_location,
        from_local_storage,
    };

    // Determine which loans originate from input borrow parameters vs local storage.
    // A loan `&(*p)` originates from local storage iff `p` may hold any local-storage loan,
    // or `p` is not derived from an input parameter.
    let reaching = compute_reaching_loans(body, cfg, &table);
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..table.loans.len() {
            let loan = &table.loans[i];
            let starts_with_deref = matches!(
                loan.place.projection.first(),
                Some(ProjectionElem::Deref)
            );
            if !starts_with_deref {
                continue;
            }
            let base = loan.place.local;
            let is_arg = body
                .local_decl(base)
                .is_some_and(|d| d.kind == LocalKind::Arg);
            if is_arg {
                if table.from_local_storage[i] {
                    table.from_local_storage[i] = false;
                    changed = true;
                }
                continue;
            }

            // Non-arg local: check if any reaching loan on `base` at `loan.issued_at.block`
            // is from local storage, or if `base` holds no loans at all (e.g. uninit/raw).
            let block_reaching = reaching
                .get(loan.issued_at.block.index())
                .and_then(|v| v.get(base.index()));
            let holds_local = block_reaching
                .is_some_and(|set| set.iter().any(|&id| table.from_local_storage[id]));
            if !holds_local
                && block_reaching.is_some_and(|set| !set.is_empty())
                && table.from_local_storage[i]
            {
                table.from_local_storage[i] = false;
                changed = true;
            }
        }
    }

    table
}

fn compute_reaching_loans(
    body: &Body,
    cfg: &Cfg,
    loans: &LoanTable,
) -> Vec<Vec<BTreeSet<usize>>> {
    let num_blocks = body.basic_blocks.len();
    let num_locals = body.locals.len();

    let mut entry_states = vec![vec![BTreeSet::new(); num_locals]; num_blocks];
    let mut exit_states = vec![vec![BTreeSet::new(); num_locals]; num_blocks];

    if num_blocks == 0 {
        return entry_states;
    }

    let mut initialized_block = vec![false; num_blocks];
    let mut worklist: VecDeque<BasicBlockId> = cfg.rpo.iter().copied().collect();
    let mut in_queue = vec![false; num_blocks];
    for &bb in &cfg.rpo {
        in_queue[bb.index()] = true;
    }

    while let Some(bb_id) = worklist.pop_front() {
        let idx = bb_id.index();
        in_queue[idx] = false;
        if !cfg.is_reachable(bb_id) {
            continue;
        }

        let mut new_entry = vec![BTreeSet::new(); num_locals];
        for &pred in cfg.predecessors(bb_id) {
            if !initialized_block[pred.index()] {
                continue;
            }
            if let Some(pred_exit) = exit_states.get(pred.index()) {
                for (acc, src) in new_entry.iter_mut().zip(pred_exit.iter()) {
                    acc.extend(src.iter().copied());
                }
            }
        }

        entry_states[idx] = new_entry.clone();
        let mut state = new_entry;
        if let Some(bb) = body.basic_blocks.get(idx) {
            for (stmt_idx, stmt) in bb.statements.iter().enumerate() {
                let loc = Location::new(bb_id, stmt_idx);
                transfer_reaching_stmt(&stmt.kind, loc, loans, &mut state);
            }
        }

        let changed = !initialized_block[idx] || exit_states[idx] != state;
        initialized_block[idx] = true;
        if changed {
            exit_states[idx] = state;
            for &succ in cfg.successors(bb_id) {
                if !in_queue[succ.index()] {
                    in_queue[succ.index()] = true;
                    worklist.push_back(succ);
                }
            }
        }
    }

    entry_states
}

fn rvalue_loans(
    rvalue: &Rvalue,
    loc: Location,
    loans: &LoanTable,
    state: &[BTreeSet<usize>],
) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    match rvalue {
        Rvalue::Ref { place, .. } => {
            if let Some(&id) = loans.by_location.get(&loc) {
                out.insert(id);
            }
            if matches!(place.projection.first(), Some(ProjectionElem::Deref))
                && let Some(base_loans) = state.get(place.local.index())
            {
                out.extend(base_loans.iter().copied());
            }
        }
        Rvalue::Use(op) | Rvalue::Cast(_, op, _) => {
            if let Operand::Copy(p) | Operand::Move(p) = op
                && !p.projection.contains(&ProjectionElem::Deref)
                && let Some(src_loans) = state.get(p.local.index())
            {
                out.extend(src_loans.iter().copied());
            }
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                if let Operand::Copy(p) | Operand::Move(p) = op
                    && !p.projection.contains(&ProjectionElem::Deref)
                    && let Some(src_loans) = state.get(p.local.index())
                {
                    out.extend(src_loans.iter().copied());
                }
            }
        }
        Rvalue::BinaryOp(..) | Rvalue::UnaryOp(..) | Rvalue::Len(_) | Rvalue::Discriminant(_) => {}
    }
    out
}

fn transfer_reaching_stmt(
    kind: &StatementKind,
    loc: Location,
    loans: &LoanTable,
    state: &mut [BTreeSet<usize>],
) {
    match kind {
        StatementKind::Assign(dest, rvalue) => {
            let rhs = rvalue_loans(rvalue, loc, loans, state);
            if let Some(slot) = state.get_mut(dest.local.index()) {
                if dest.projection.is_empty() {
                    *slot = rhs;
                } else {
                    slot.extend(rhs);
                }
            }
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            if let Some(slot) = state.get_mut(local.index()) {
                slot.clear();
            }
        }
        StatementKind::SetDiscriminant { .. }
        | StatementKind::Drop(_)
        | StatementKind::Nop => {}
    }
}

fn compute_live_locals(body: &Body, cfg: &Cfg) -> Vec<BTreeSet<LocalId>> {
    let num_blocks = body.basic_blocks.len();
    let mut live_in = vec![BTreeSet::new(); num_blocks];
    let mut live_out = vec![BTreeSet::new(); num_blocks];

    if num_blocks == 0 {
        return live_out;
    }

    let mut worklist: VecDeque<BasicBlockId> = cfg.rpo.iter().rev().copied().collect();
    let mut in_queue = vec![false; num_blocks];
    for &bb in cfg.rpo.iter().rev() {
        in_queue[bb.index()] = true;
    }

    while let Some(bb_id) = worklist.pop_front() {
        let idx = bb_id.index();
        in_queue[idx] = false;
        if !cfg.is_reachable(bb_id) {
            continue;
        }

        let mut out_set = BTreeSet::new();
        for &succ in cfg.successors(bb_id) {
            if let Some(succ_in) = live_in.get(succ.index()) {
                out_set.extend(succ_in.iter().copied());
            }
        }
        live_out[idx] = out_set.clone();

        let mut live = out_set;
        if let Some(bb) = body.basic_blocks.get(idx) {
            if let Some(term) = &bb.terminator {
                transfer_live_term(&term.kind, body.locals.len(), &mut live);
            }
            for stmt in bb.statements.iter().rev() {
                transfer_live_stmt(&stmt.kind, &mut live);
            }
        }

        if live_in[idx] != live {
            live_in[idx] = live;
            for &pred in cfg.predecessors(bb_id) {
                if !in_queue[pred.index()] {
                    in_queue[pred.index()] = true;
                    worklist.push_back(pred);
                }
            }
        }
    }

    live_out
}

/// Computes `live_before` for each statement index `0..bb.statements.len()` and the terminator
/// at `bb.statements.len()`.
fn compute_block_stmt_liveness(
    bb: &crate::ir::BasicBlockData,
    local_count: usize,
    live_after_block: BTreeSet<LocalId>,
) -> Vec<BTreeSet<LocalId>> {
    let total = bb.statements.len() + 1;
    let mut result = vec![BTreeSet::new(); total];
    let mut live = live_after_block;

    if let Some(term) = &bb.terminator {
        transfer_live_term(&term.kind, local_count, &mut live);
    }
    if let Some(slot) = result.get_mut(bb.statements.len()) {
        *slot = live.clone();
    }

    for (idx, stmt) in bb.statements.iter().enumerate().rev() {
        transfer_live_stmt(&stmt.kind, &mut live);
        if let Some(slot) = result.get_mut(idx) {
            *slot = live.clone();
        }
    }

    result
}

fn collect_place_uses(place: &Place, include_base: bool, uses: &mut BTreeSet<LocalId>) {
    if include_base {
        uses.insert(place.local);
    }
    for elem in &place.projection {
        if let ProjectionElem::Index(idx_local) = elem {
            uses.insert(*idx_local);
        }
    }
}

fn collect_operand_uses(op: &Operand, uses: &mut BTreeSet<LocalId>) {
    match op {
        Operand::Copy(place) | Operand::Move(place) => collect_place_uses(place, true, uses),
        Operand::Constant(_) => {}
    }
}

fn collect_rvalue_uses(rvalue: &Rvalue, uses: &mut BTreeSet<LocalId>) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            collect_operand_uses(op, uses);
        }
        Rvalue::Ref { place, .. } | Rvalue::Len(place) | Rvalue::Discriminant(place) => {
            collect_place_uses(place, true, uses);
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            collect_operand_uses(lhs, uses);
            collect_operand_uses(rhs, uses);
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                collect_operand_uses(op, uses);
            }
        }
    }
}

fn transfer_live_stmt(kind: &StatementKind, live: &mut BTreeSet<LocalId>) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            let mut uses = BTreeSet::new();
            collect_rvalue_uses(rvalue, &mut uses);
            if place.projection.is_empty() {
                live.remove(&place.local);
            } else {
                collect_place_uses(place, true, &mut uses);
            }
            live.extend(uses);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            live.remove(local);
        }
        StatementKind::SetDiscriminant { place, .. } | StatementKind::Drop(place) => {
            collect_place_uses(place, true, live);
        }
        StatementKind::Nop => {}
    }
}

fn transfer_live_term(kind: &TerminatorKind, local_count: usize, live: &mut BTreeSet<LocalId>) {
    match kind {
        TerminatorKind::Return => {
            if local_count > 0 {
                live.insert(LocalId::RETURN_PLACE);
            }
        }
        TerminatorKind::SwitchInt { discr, .. } => collect_operand_uses(discr, live),
        TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } => {
            let mut uses = BTreeSet::new();
            collect_operand_uses(func, &mut uses);
            for arg in args {
                collect_operand_uses(arg, &mut uses);
            }
            if destination.projection.is_empty() {
                live.remove(&destination.local);
            } else {
                collect_place_uses(destination, true, &mut uses);
            }
            live.extend(uses);
        }
        TerminatorKind::Drop { place, .. } => collect_place_uses(place, true, live),
        TerminatorKind::Assert { cond, msg, .. } => {
            collect_operand_uses(cond, live);
            match msg {
                AssertMessage::BoundsCheck { len, index } => {
                    collect_operand_uses(len, live);
                    collect_operand_uses(index, live);
                }
                AssertMessage::DivisionByZero(op) | AssertMessage::RemainderByZero(op) => {
                    collect_operand_uses(op, live);
                }
                AssertMessage::Overflow(_, lhs, rhs) => {
                    collect_operand_uses(lhs, live);
                    collect_operand_uses(rhs, live);
                }
                AssertMessage::Custom(_) => {}
            }
        }
        TerminatorKind::Goto { .. } | TerminatorKind::Unreachable => {}
    }
}

// ============================================================================
// Conflict & initialization checking at each program point
// ============================================================================

fn active_loans_at(
    live_locals: &BTreeSet<LocalId>,
    reaching_state: &[BTreeSet<usize>],
) -> BTreeSet<usize> {
    let mut active = BTreeSet::new();
    for &local in live_locals {
        if let Some(loans) = reaching_state.get(local.index()) {
            active.extend(loans.iter().copied());
        }
    }
    active
}

/// Returns `true` if `loan_place` and `access_place` may refer to overlapping memory.
fn places_conflict(loan_place: &Place, access_place: &Place) -> bool {
    if loan_place.local != access_place.local {
        return false;
    }

    let mut i = 0;
    let p1 = &loan_place.projection;
    let p2 = &access_place.projection;

    while i < p1.len() && i < p2.len() {
        match (p1[i], p2[i]) {
            (ProjectionElem::Field(a), ProjectionElem::Field(b)) => {
                if a != b {
                    return false;
                }
                i += 1;
            }
            (ProjectionElem::Downcast(a), ProjectionElem::Downcast(b)) => {
                if a != b {
                    return false;
                }
                i += 1;
            }
            (ProjectionElem::Deref, ProjectionElem::Deref) => {
                i += 1;
            }
            (ProjectionElem::Index(_), _) | (_, ProjectionElem::Index(_)) => {
                return true;
            }
            _ => return true,
        }
    }

    true
}

fn access_conflicts_with_loan(access: AccessKind, loan_kind: BorrowKind) -> bool {
    match loan_kind {
        BorrowKind::Shared => matches!(
            access,
            AccessKind::Write | AccessKind::Move | AccessKind::Borrow(BorrowKind::Mut)
        ),
        BorrowKind::Mut => true,
    }
}

fn check_place_init(
    place: &Place,
    loc: Location,
    init_state: &[LocalInitTree],
    errors: &mut Vec<BorrowError>,
) {
    // Check any index locals used in projections.
    for elem in &place.projection {
        if let ProjectionElem::Index(idx_local) = elem {
            let idx_place = Place::from_local(*idx_local);
            check_place_init(&idx_place, loc, init_state, errors);
        }
    }

    let info = extract_place_path(place);
    let Some(tree) = init_state.get(place.local.index()) else {
        return;
    };

    let state = tree.get_path(&info.fields);
    match state {
        PathInitState::Init => {}
        PathInitState::Uninit => errors.push(BorrowError {
            location: loc,
            kind: BorrowErrorKind::UseOfUninitialized {
                place: place.clone(),
            },
        }),
        PathInitState::Moved => errors.push(BorrowError {
            location: loc,
            kind: BorrowErrorKind::UseOfMoved {
                place: place.clone(),
            },
        }),
        PathInitState::MaybeInit => errors.push(BorrowError {
            location: loc,
            kind: BorrowErrorKind::UseOfMaybeUninitialized {
                place: place.clone(),
            },
        }),
    }
}

fn check_place_loan_conflicts(
    place: &Place,
    access: AccessKind,
    loc: Location,
    active_loans: &BTreeSet<usize>,
    loans: &LoanTable,
    errors: &mut Vec<BorrowError>,
) {
    for &loan_id in active_loans {
        let Some(loan) = loans.loans.get(loan_id) else {
            continue;
        };
        if access_conflicts_with_loan(access, loan.kind) && places_conflict(&loan.place, place) {
            errors.push(BorrowError {
                location: loc,
                kind: BorrowErrorKind::BorrowConflict {
                    access,
                    place: place.clone(),
                    loan: loan.clone(),
                },
            });
        }
    }
}

fn check_place_indices_access(
    place: &Place,
    loc: Location,
    active_loans: &BTreeSet<usize>,
    loans: &LoanTable,
    init_state: &[LocalInitTree],
    errors: &mut Vec<BorrowError>,
) {
    for elem in &place.projection {
        if let ProjectionElem::Index(idx_local) = elem {
            let idx_place = Place::from_local(*idx_local);
            check_place_init(&idx_place, loc, init_state, errors);
            check_place_loan_conflicts(
                &idx_place,
                AccessKind::Read,
                loc,
                active_loans,
                loans,
                errors,
            );
        }
    }
}

fn check_operand(
    op: &Operand,
    loc: Location,
    active_loans: &BTreeSet<usize>,
    loans: &LoanTable,
    init_state: &[LocalInitTree],
    errors: &mut Vec<BorrowError>,
) {
    match op {
        Operand::Copy(place) => {
            check_place_init(place, loc, init_state, errors);
            check_place_indices_access(place, loc, active_loans, loans, init_state, errors);
            check_place_loan_conflicts(
                place,
                AccessKind::Read,
                loc,
                active_loans,
                loans,
                errors,
            );
        }
        Operand::Move(place) => {
            check_place_init(place, loc, init_state, errors);
            check_place_indices_access(place, loc, active_loans, loans, init_state, errors);
            check_place_loan_conflicts(
                place,
                AccessKind::Move,
                loc,
                active_loans,
                loans,
                errors,
            );
        }
        Operand::Constant(_) => {}
    }
}

fn check_rvalue(
    rvalue: &Rvalue,
    loc: Location,
    active_loans: &BTreeSet<usize>,
    loans: &LoanTable,
    init_state: &[LocalInitTree],
    errors: &mut Vec<BorrowError>,
) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            check_operand(op, loc, active_loans, loans, init_state, errors);
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            check_operand(lhs, loc, active_loans, loans, init_state, errors);
            check_operand(rhs, loc, active_loans, loans, init_state, errors);
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                check_operand(op, loc, active_loans, loans, init_state, errors);
            }
        }
        Rvalue::Ref { borrow_kind, place } => {
            check_place_init(place, loc, init_state, errors);
            check_place_indices_access(place, loc, active_loans, loans, init_state, errors);
            check_place_loan_conflicts(
                place,
                AccessKind::Borrow(*borrow_kind),
                loc,
                active_loans,
                loans,
                errors,
            );
        }
        Rvalue::Len(place) | Rvalue::Discriminant(place) => {
            check_place_init(place, loc, init_state, errors);
            check_place_indices_access(place, loc, active_loans, loans, init_state, errors);
            check_place_loan_conflicts(
                place,
                AccessKind::Read,
                loc,
                active_loans,
                loans,
                errors,
            );
        }
    }
}

fn check_write_place(
    place: &Place,
    loc: Location,
    active_loans: &BTreeSet<usize>,
    loans: &LoanTable,
    init_state: &[LocalInitTree],
    errors: &mut Vec<BorrowError>,
) {
    check_place_indices_access(place, loc, active_loans, loans, init_state, errors);
    let info = extract_place_path(place);
    if info.has_deref || info.has_index {
        let base_place = Place::from_local(place.local);
        check_place_init(&base_place, loc, init_state, errors);
    }
    check_place_loan_conflicts(
        place,
        AccessKind::Write,
        loc,
        active_loans,
        loans,
        errors,
    );
}

fn check_and_step_statement(
    kind: &StatementKind,
    loc: Location,
    loans: &LoanTable,
    live_before: &BTreeSet<LocalId>,
    init_state: &mut [LocalInitTree],
    reaching_state: &mut [BTreeSet<usize>],
    errors: &mut Vec<BorrowError>,
) {
    let active = active_loans_at(live_before, reaching_state);

    match kind {
        StatementKind::Assign(dest, rvalue) => {
            check_rvalue(rvalue, loc, &active, loans, init_state, errors);
            check_write_place(dest, loc, &active, loans, init_state, errors);
        }
        StatementKind::SetDiscriminant { place, .. } => {
            check_write_place(place, loc, &active, loans, init_state, errors);
        }
        StatementKind::Drop(place) => {
            check_place_indices_access(place, loc, &active, loans, init_state, errors);
            check_place_loan_conflicts(place, AccessKind::Move, loc, &active, loans, errors);
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
    }

    transfer_init_stmt(kind, init_state);
    transfer_reaching_stmt(kind, loc, loans, reaching_state);
}

#[allow(clippy::too_many_arguments)]
fn check_and_step_terminator(
    body: &Body,
    kind: &TerminatorKind,
    loc: Location,
    loans: &LoanTable,
    live_before: &BTreeSet<LocalId>,
    init_state: &mut [LocalInitTree],
    reaching_state: &mut [BTreeSet<usize>],
    errors: &mut Vec<BorrowError>,
) {
    let active = active_loans_at(live_before, reaching_state);

    match kind {
        TerminatorKind::SwitchInt { discr, .. } => {
            check_operand(discr, loc, &active, loans, init_state, errors);
        }
        TerminatorKind::Call {
            func,
            args,
            destination,
            target,
        } => {
            check_operand(func, loc, &active, loans, init_state, errors);
            for arg in args {
                check_operand(arg, loc, &active, loans, init_state, errors);
            }
            if target.is_some() {
                check_write_place(destination, loc, &active, loans, init_state, errors);
            }
        }
        TerminatorKind::Drop { place, .. } => {
            check_place_indices_access(place, loc, &active, loans, init_state, errors);
            check_place_loan_conflicts(place, AccessKind::Move, loc, &active, loans, errors);
        }
        TerminatorKind::Assert { cond, msg, .. } => {
            check_operand(cond, loc, &active, loans, init_state, errors);
            match msg {
                AssertMessage::BoundsCheck { len, index } => {
                    check_operand(len, loc, &active, loans, init_state, errors);
                    check_operand(index, loc, &active, loans, init_state, errors);
                }
                AssertMessage::DivisionByZero(op) | AssertMessage::RemainderByZero(op) => {
                    check_operand(op, loc, &active, loans, init_state, errors);
                }
                AssertMessage::Overflow(_, lhs, rhs) => {
                    check_operand(lhs, loc, &active, loans, init_state, errors);
                    check_operand(rhs, loc, &active, loans, init_state, errors);
                }
                AssertMessage::Custom(_) => {}
            }
        }
        TerminatorKind::Return => {
            if let Some(ret_decl) = body.local_decl(LocalId::RETURN_PLACE)
                && ret_decl.ty != TyKind::Unit
            {
                check_place_init(
                    &Place::from_local(LocalId::RETURN_PLACE),
                    loc,
                    init_state,
                    errors,
                );
            }
            if let Some(ret_loans) = reaching_state.get(LocalId::RETURN_PLACE.index()) {
                for &loan_id in ret_loans {
                    if loans.from_local_storage.get(loan_id).copied().unwrap_or(false)
                        && let Some(loan) = loans.loans.get(loan_id)
                    {
                        errors.push(BorrowError {
                            location: loc,
                            kind: BorrowErrorKind::EscapingReference {
                                local: LocalId::RETURN_PLACE,
                                loan: loan.clone(),
                            },
                        });
                    }
                }
            }
        }
        TerminatorKind::Goto { .. } | TerminatorKind::Unreachable => {}
    }

    transfer_init_term(kind, init_state);
}
