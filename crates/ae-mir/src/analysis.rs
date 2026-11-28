//! Pure MIR control-flow and dataflow analyses (P3.1/P3.2).
//!
//! Provides CFG predecessor/successor maps, reverse-postorder (RPO) traversal,
//! forward local initialization dataflow (`Uninit` / `Init` / `MaybeInit`),
//! and backward live-locals analysis.

use std::collections::{BTreeSet, VecDeque};

use crate::ir::{
    AssertMessage, BasicBlockId, Body, LocalId, Operand, Place, ProjectionElem, Rvalue,
    StatementKind, TerminatorKind,
};

/// Control-flow graph adjacency, reachability, and reverse-postorder traversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cfg {
    pub predecessors: Vec<Vec<BasicBlockId>>,
    pub successors: Vec<Vec<BasicBlockId>>,
    pub reachable: Vec<bool>,
    pub rpo: Vec<BasicBlockId>,
}

impl Cfg {
    /// Computes CFG predecessors, successors, reachability from `bb0`, and RPO.
    pub fn compute(body: &Body) -> Self {
        let n = body.basic_blocks.len();
        let mut successors = vec![Vec::new(); n];
        let mut predecessors = vec![Vec::new(); n];

        for (idx, bb) in body.basic_blocks.iter().enumerate() {
            let from = BasicBlockId(idx as u32);
            if let Some(term) = &bb.terminator {
                for succ in term.kind.successors() {
                    if succ.index() < n {
                        if !successors[idx].contains(&succ) {
                            successors[idx].push(succ);
                        }
                        if !predecessors[succ.index()].contains(&from) {
                            predecessors[succ.index()].push(from);
                        }
                    }
                }
            }
        }

        let mut reachable = vec![false; n];
        let mut rpo = Vec::new();
        if n > 0 {
            let mut postorder = Vec::new();
            dfs_postorder(
                BasicBlockId::ENTRY,
                &successors,
                &mut reachable,
                &mut postorder,
            );
            postorder.reverse();
            rpo = postorder;
        }

        Self {
            predecessors,
            successors,
            reachable,
            rpo,
        }
    }

    pub fn predecessors(&self, bb: BasicBlockId) -> &[BasicBlockId] {
        self.predecessors
            .get(bb.index())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn successors(&self, bb: BasicBlockId) -> &[BasicBlockId] {
        self.successors
            .get(bb.index())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn rpo(&self) -> &[BasicBlockId] {
        &self.rpo
    }

    pub fn is_reachable(&self, bb: BasicBlockId) -> bool {
        self.reachable.get(bb.index()).copied().unwrap_or(false)
    }
}

fn dfs_postorder(
    bb: BasicBlockId,
    successors: &[Vec<BasicBlockId>],
    visited: &mut [bool],
    postorder: &mut Vec<BasicBlockId>,
) {
    // Iterative DFS to avoid stack overflow on deep CFGs.
    enum Frame {
        Enter(BasicBlockId),
        Exit(BasicBlockId),
    }

    let mut stack = vec![Frame::Enter(bb)];
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Enter(node) => {
                let idx = node.index();
                if idx >= visited.len() || visited[idx] {
                    continue;
                }
                visited[idx] = true;
                stack.push(Frame::Exit(node));
                for &succ in successors[idx].iter().rev() {
                    if succ.index() < visited.len() && !visited[succ.index()] {
                        stack.push(Frame::Enter(succ));
                    }
                }
            }
            Frame::Exit(node) => {
                postorder.push(node);
            }
        }
    }
}

/// Abstract initialization state of a local variable at a program point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InitState {
    Uninit,
    Init,
    MaybeInit,
}

impl InitState {
    /// Lattice join across incoming CFG edges.
    pub fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::MaybeInit
        }
    }
}

/// Forward definite/maybe local initialization analysis across CFG edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitAnalysis {
    pub entry_states: Vec<Vec<InitState>>,
    pub exit_states: Vec<Vec<InitState>>,
}

impl InitAnalysis {
    /// Computes per-block entry and exit [`InitState`] for every local in `body`.
    pub fn analyze(body: &Body) -> Self {
        let cfg = Cfg::compute(body);
        let num_blocks = body.basic_blocks.len();
        let num_locals = body.locals.len();

        let mut entry_states = vec![vec![InitState::Uninit; num_locals]; num_blocks];
        let mut exit_states = vec![vec![InitState::Uninit; num_locals]; num_blocks];

        if num_blocks == 0 {
            return Self {
                entry_states,
                exit_states,
            };
        }

        // Arguments (_1..=arg_count) are initialized on function entry.
        let mut init_entry = vec![InitState::Uninit; num_locals];
        let arg_end = body.arg_count.min(num_locals.saturating_sub(1));
        init_entry[1..=arg_end].fill(InitState::Init);
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

            let new_entry = if idx == 0 && cfg.predecessors[0].is_empty() {
                entry_states[0].clone()
            } else {
                let mut joined: Option<Vec<InitState>> = if idx == 0 {
                    Some(entry_states[0].clone())
                } else {
                    None
                };
                for &pred in &cfg.predecessors[idx] {
                    if !initialized_block[pred.index()] {
                        continue;
                    }
                    let pred_exit = &exit_states[pred.index()];
                    match &mut joined {
                        None => joined = Some(pred_exit.clone()),
                        Some(acc) => {
                            for (a, &b) in acc.iter_mut().zip(pred_exit.iter()) {
                                *a = a.join(b);
                            }
                        }
                    }
                }
                joined.unwrap_or_else(|| vec![InitState::Uninit; num_locals])
            };

            entry_states[idx] = new_entry.clone();
            let mut state = new_entry;
            let bb = &body.basic_blocks[idx];
            for stmt in &bb.statements {
                transfer_init_stmt(&stmt.kind, &mut state);
            }
            if let Some(term) = &bb.terminator {
                transfer_init_term(&term.kind, &mut state);
            }

            let changed = !initialized_block[idx] || exit_states[idx] != state;
            initialized_block[idx] = true;
            if changed {
                exit_states[idx] = state;
                for &succ in &cfg.successors[idx] {
                    if !in_queue[succ.index()] {
                        in_queue[succ.index()] = true;
                        worklist.push_back(succ);
                    }
                }
            }
        }

        Self {
            entry_states,
            exit_states,
        }
    }

    pub fn entry(&self, bb: BasicBlockId) -> &[InitState] {
        self.entry_states
            .get(bb.index())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn exit(&self, bb: BasicBlockId) -> &[InitState] {
        self.exit_states
            .get(bb.index())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn is_definitely_init(&self, bb: BasicBlockId, local: LocalId) -> bool {
        self.entry(bb).get(local.index()) == Some(&InitState::Init)
    }

    pub fn is_maybe_init(&self, bb: BasicBlockId, local: LocalId) -> bool {
        matches!(
            self.entry(bb).get(local.index()),
            Some(InitState::Init | InitState::MaybeInit)
        )
    }
}

fn mark_operand_moves(op: &Operand, state: &mut [InitState]) {
    if let Operand::Move(place) = op
        && let Some(slot) = state.get_mut(place.local.index())
    {
        if place.projection.is_empty() {
            *slot = InitState::Uninit;
        } else if *slot == InitState::Init {
            *slot = InitState::MaybeInit;
        }
    }
}

fn mark_rvalue_moves(rvalue: &Rvalue, state: &mut [InitState]) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            mark_operand_moves(op, state);
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            mark_operand_moves(lhs, state);
            mark_operand_moves(rhs, state);
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                mark_operand_moves(op, state);
            }
        }
        Rvalue::Ref { .. } | Rvalue::AddressOf(_) | Rvalue::Len(_) | Rvalue::Discriminant(_) => {}
    }
}

fn mark_place_write(place: &Place, state: &mut [InitState]) {
    if let Some(slot) = state.get_mut(place.local.index()) {
        if place.projection.is_empty() {
            *slot = InitState::Init;
        } else if *slot == InitState::Uninit {
            *slot = InitState::MaybeInit;
        }
    }
}

fn mark_place_drop(place: &Place, state: &mut [InitState]) {
    if let Some(slot) = state.get_mut(place.local.index()) {
        if place.projection.is_empty() {
            *slot = InitState::Uninit;
        } else if *slot == InitState::Init {
            *slot = InitState::MaybeInit;
        }
    }
}

fn transfer_init_stmt(kind: &StatementKind, state: &mut [InitState]) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            mark_rvalue_moves(rvalue, state);
            mark_place_write(place, state);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            if let Some(slot) = state.get_mut(local.index()) {
                *slot = InitState::Uninit;
            }
        }
        StatementKind::SetDiscriminant { place, .. } => {
            if let Some(slot) = state.get_mut(place.local.index())
                && *slot == InitState::Uninit
            {
                *slot = InitState::MaybeInit;
            }
        }
        StatementKind::Drop(place) => mark_place_drop(place, state),
        StatementKind::Nop => {}
    }
}

fn transfer_init_term(kind: &TerminatorKind, state: &mut [InitState]) {
    match kind {
        TerminatorKind::SwitchInt { discr, .. } => mark_operand_moves(discr, state),
        TerminatorKind::Call {
            func,
            args,
            destination,
            target,
        } => {
            mark_operand_moves(func, state);
            for arg in args {
                mark_operand_moves(arg, state);
            }
            if target.is_some() {
                mark_place_write(destination, state);
            }
        }
        TerminatorKind::Drop { place, .. } => mark_place_drop(place, state),
        TerminatorKind::Assert { cond, .. } => mark_operand_moves(cond, state),
        TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => {}
    }
}

/// Backward live-locals dataflow analysis over basic blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivenessAnalysis {
    pub live_in: Vec<BTreeSet<LocalId>>,
    pub live_out: Vec<BTreeSet<LocalId>>,
}

impl LivenessAnalysis {
    /// Computes `live_in` and `live_out` local sets for every basic block in `body`.
    pub fn analyze(body: &Body) -> Self {
        let cfg = Cfg::compute(body);
        let num_blocks = body.basic_blocks.len();
        let mut live_in = vec![BTreeSet::new(); num_blocks];
        let mut live_out = vec![BTreeSet::new(); num_blocks];

        if num_blocks == 0 {
            return Self { live_in, live_out };
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
            for &succ in &cfg.successors[idx] {
                for &local in &live_in[succ.index()] {
                    out_set.insert(local);
                }
            }
            live_out[idx] = out_set.clone();

            let mut live = out_set;
            let bb = &body.basic_blocks[idx];
            if let Some(term) = &bb.terminator {
                transfer_live_term(&term.kind, body.locals.len(), &mut live);
            }
            for stmt in bb.statements.iter().rev() {
                transfer_live_stmt(&stmt.kind, &mut live);
            }

            if live_in[idx] != live {
                live_in[idx] = live;
                for &pred in &cfg.predecessors[idx] {
                    if !in_queue[pred.index()] {
                        in_queue[pred.index()] = true;
                        worklist.push_back(pred);
                    }
                }
            }
        }

        Self { live_in, live_out }
    }

    pub fn live_in(&self, bb: BasicBlockId) -> &BTreeSet<LocalId> {
        static EMPTY: BTreeSet<LocalId> = BTreeSet::new();
        self.live_in.get(bb.index()).unwrap_or(&EMPTY)
    }

    pub fn live_out(&self, bb: BasicBlockId) -> &BTreeSet<LocalId> {
        static EMPTY: BTreeSet<LocalId> = BTreeSet::new();
        self.live_out.get(bb.index()).unwrap_or(&EMPTY)
    }

    pub fn is_live_in(&self, bb: BasicBlockId, local: LocalId) -> bool {
        self.live_in(bb).contains(&local)
    }

    pub fn is_live_out(&self, bb: BasicBlockId, local: LocalId) -> bool {
        self.live_out(bb).contains(&local)
    }
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
        Rvalue::Ref { place, .. } | Rvalue::AddressOf(place) | Rvalue::Len(place) | Rvalue::Discriminant(place) => {
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
