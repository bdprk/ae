//! Pure MIR optimization passes (P3.1/P3.2).
//!
//! Includes unreachable-block pruning with `BasicBlockId` compaction,
//! trivial `Goto` jump-chain compression, and local constant folding /
//! propagation within basic blocks. Every pass preserves `ae_mir::validate`.

use std::collections::HashMap;

use ae_ty::{FloatTy, IntTy, TyKind};

use crate::analysis::{Cfg, LivenessAnalysis};
use crate::ir::{
    BasicBlockId, BinOp, Body, ConstFloat, ConstValue, Constant, LocalId, Operand, Place,
    ProjectionElem, Rvalue, StatementKind, TerminatorKind, UnOp,
};

/// Summary statistics produced by [`optimize_body`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OptStats {
    pub blocks_pruned: usize,
    pub jumps_threaded: usize,
    pub constants_folded: usize,
    pub copies_propagated: usize,
}

impl OptStats {
    pub fn total_changes(&self) -> usize {
        self.blocks_pruned
            + self.jumps_threaded
            + self.constants_folded
            + self.copies_propagated
    }
}

/// Runs local constant folding/propagation, jump-chain compression, and
/// unreachable-block pruning to a fixed point while preserving `validate(body)`.
pub fn optimize_body(body: &mut Body) -> OptStats {
    let mut total = OptStats::default();
    if body.basic_blocks.is_empty() {
        return total;
    }

    for _ in 0..8 {
        let mut changed = false;

        let (folded, propagated) = fold_constants_and_propagate(body);
        if folded > 0 || propagated > 0 {
            total.constants_folded += folded;
            total.copies_propagated += propagated;
            changed = true;
        }

        let threaded = compress_goto_chains(body);
        if threaded > 0 {
            total.jumps_threaded += threaded;
            changed = true;
        }

        let pruned = prune_unreachable_blocks(body);
        if pruned > 0 {
            total.blocks_pruned += pruned;
            changed = true;
        }

        if !changed {
            break;
        }
    }

    total
}

/// Removes basic blocks unreachable from `bb0` and compacts `BasicBlockId`s.
pub fn prune_unreachable_blocks(body: &mut Body) -> usize {
    if body.basic_blocks.is_empty() {
        return 0;
    }

    let cfg = Cfg::compute(body);
    let old_len = body.basic_blocks.len();
    let mut remap = vec![None; old_len];
    let mut next_id = 0u32;

    for (idx, &is_reach) in cfg.reachable.iter().enumerate() {
        if is_reach {
            remap[idx] = Some(BasicBlockId(next_id));
            next_id += 1;
        }
    }

    let pruned = old_len - next_id as usize;
    if pruned == 0 {
        return 0;
    }

    let mut new_blocks = Vec::with_capacity(next_id as usize);
    for (idx, mut bb) in body.basic_blocks.drain(..).enumerate() {
        if remap[idx].is_some() {
            if let Some(term) = &mut bb.terminator {
                remap_terminator_targets(&mut term.kind, &remap);
            }
            new_blocks.push(bb);
        }
    }
    body.basic_blocks = new_blocks;
    pruned
}

/// Threads edges through empty `Goto`-only basic blocks.
pub fn compress_goto_chains(body: &mut Body) -> usize {
    let n = body.basic_blocks.len();
    if n == 0 {
        return 0;
    }

    let mut resolved = Vec::with_capacity(n);
    for idx in 0..n {
        resolved.push(resolve_goto_target(body, BasicBlockId(idx as u32)));
    }

    let mut threaded = 0usize;
    for bb in &mut body.basic_blocks {
        if let Some(term) = &mut bb.terminator {
            threaded += thread_terminator_targets(&mut term.kind, &resolved);
        }
    }
    threaded
}

fn resolve_goto_target(body: &Body, start: BasicBlockId) -> BasicBlockId {
    let n = body.basic_blocks.len();
    let mut visited = vec![false; n];
    let mut cur = start;

    while cur.index() < n && !visited[cur.index()] {
        visited[cur.index()] = true;
        let bb = &body.basic_blocks[cur.index()];
        if bb.statements.is_empty()
            && let Some(term) = &bb.terminator
            && let TerminatorKind::Goto { target } = term.kind
            && target != cur
        {
            cur = target;
            continue;
        }
        break;
    }

    cur
}

fn remap_target(target: &mut BasicBlockId, remap: &[Option<BasicBlockId>]) {
    if let Some(Some(new_id)) = remap.get(target.index()) {
        *target = *new_id;
    }
}

fn remap_terminator_targets(kind: &mut TerminatorKind, remap: &[Option<BasicBlockId>]) {
    match kind {
        TerminatorKind::Goto { target } | TerminatorKind::Assert { target, .. } => {
            remap_target(target, remap);
        }
        TerminatorKind::SwitchInt { targets, .. } => {
            for (_, target) in &mut targets.cases {
                remap_target(target, remap);
            }
            remap_target(&mut targets.otherwise, remap);
        }
        TerminatorKind::Call { target, .. } => {
            if let Some(t) = target {
                remap_target(t, remap);
            }
        }
        TerminatorKind::Drop { target, unwind, .. } => {
            remap_target(target, remap);
            if let Some(u) = unwind {
                remap_target(u, remap);
            }
        }
        TerminatorKind::Return | TerminatorKind::Unreachable => {}
    }
}

fn thread_target(target: &mut BasicBlockId, resolved: &[BasicBlockId]) -> usize {
    if let Some(&new_target) = resolved.get(target.index())
        && new_target != *target
    {
        *target = new_target;
        1
    } else {
        0
    }
}

fn thread_terminator_targets(kind: &mut TerminatorKind, resolved: &[BasicBlockId]) -> usize {
    match kind {
        TerminatorKind::Goto { target } | TerminatorKind::Assert { target, .. } => {
            thread_target(target, resolved)
        }
        TerminatorKind::SwitchInt { targets, .. } => {
            let mut count = 0;
            for (_, target) in &mut targets.cases {
                count += thread_target(target, resolved);
            }
            count += thread_target(&mut targets.otherwise, resolved);
            count
        }
        TerminatorKind::Call { target, .. } => {
            if let Some(t) = target {
                thread_target(t, resolved)
            } else {
                0
            }
        }
        TerminatorKind::Drop { target, unwind, .. } => {
            let mut count = thread_target(target, resolved);
            if let Some(u) = unwind {
                count += thread_target(u, resolved);
            }
            count
        }
        TerminatorKind::Return | TerminatorKind::Unreachable => 0,
    }
}

/// Performs intra-block constant propagation and folding.
///
/// Preserves trapping operations (division/remainder by zero, integer overflow,
/// failed assertions, and observable use-after-move) so optimization never
/// changes trap behavior.
pub fn fold_constants_and_propagate(body: &mut Body) -> (usize, usize) {
    let liveness = LivenessAnalysis::analyze(body);
    let mut folded = 0usize;
    let mut propagated = 0usize;

    for bb_idx in 0..body.basic_blocks.len() {
        let bb_id = BasicBlockId(bb_idx as u32);
        let live_out = liveness.live_out(bb_id);
        let mut const_env: HashMap<LocalId, Constant> = HashMap::new();

        let stmt_len = body.basic_blocks[bb_idx].statements.len();
        for stmt_idx in 0..stmt_len {
            let mut move_invalidations = Vec::new();
            let stmt_kind = body.basic_blocks[bb_idx].statements[stmt_idx].kind.clone();

            match stmt_kind {
                StatementKind::Assign(ref place, ref rvalue) => {
                    let mut new_rvalue = rvalue.clone();
                    propagated += propagate_rvalue(
                        &mut new_rvalue,
                        &const_env,
                        &body.basic_blocks[bb_idx].statements[stmt_idx + 1..],
                        body.basic_blocks[bb_idx].terminator.as_ref(),
                        live_out,
                        &mut move_invalidations,
                    );

                    for local in move_invalidations {
                        const_env.remove(&local);
                    }

                    if let Some(const_val) = try_fold_rvalue(&new_rvalue) {
                        new_rvalue = Rvalue::Use(Operand::Constant(const_val));
                        folded += 1;
                    }

                    if place.projection.is_empty() {
                        if let Rvalue::Use(Operand::Constant(c)) = &new_rvalue {
                            const_env.insert(place.local, c.clone());
                        } else {
                            const_env.remove(&place.local);
                        }
                    } else {
                        const_env.remove(&place.local);
                    }

                    body.basic_blocks[bb_idx].statements[stmt_idx].kind =
                        StatementKind::Assign(place.clone(), new_rvalue);
                }
                StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                    const_env.remove(&local);
                }
                StatementKind::SetDiscriminant { ref place, .. }
                | StatementKind::Drop(ref place) => {
                    const_env.remove(&place.local);
                }
                StatementKind::Nop => {}
            }
        }

        // Remove Nop statements if any.
        let before_nop = body.basic_blocks[bb_idx].statements.len();
        body.basic_blocks[bb_idx]
            .statements
            .retain(|s| !matches!(s.kind, StatementKind::Nop));
        let _ = before_nop;

        if let Some(mut term) = body.basic_blocks[bb_idx].terminator.take() {
            match &mut term.kind {
                TerminatorKind::SwitchInt { discr, targets } => {
                    let mut inv = Vec::new();
                    propagated +=
                        propagate_operand(discr, &const_env, &[], None, live_out, &mut inv);
                    if let Operand::Constant(c) = discr
                        && let Some(key) = const_switch_key(c)
                    {
                        let target = targets
                            .cases
                            .iter()
                            .find(|(k, _)| *k == key)
                            .map(|(_, t)| *t)
                            .unwrap_or(targets.otherwise);
                        term.kind = TerminatorKind::Goto { target };
                        folded += 1;
                    }
                }
                TerminatorKind::Assert {
                    cond,
                    expected,
                    target,
                    ..
                } => {
                    let mut inv = Vec::new();
                    propagated +=
                        propagate_operand(cond, &const_env, &[], None, live_out, &mut inv);
                    if let Operand::Constant(c) = cond
                        && c.value == ConstValue::Bool(*expected)
                    {
                        term.kind = TerminatorKind::Goto { target: *target };
                        folded += 1;
                    }
                }
                TerminatorKind::Call { func, args, .. } => {
                    let mut inv = Vec::new();
                    propagated +=
                        propagate_operand(func, &const_env, &[], None, live_out, &mut inv);
                    for arg in args {
                        propagated +=
                            propagate_operand(arg, &const_env, &[], None, live_out, &mut inv);
                    }
                }
                TerminatorKind::Goto { .. }
                | TerminatorKind::Drop { .. }
                | TerminatorKind::Return
                | TerminatorKind::Unreachable => {}
            }
            body.basic_blocks[bb_idx].terminator = Some(term);
        }
    }

    (folded, propagated)
}

fn propagate_rvalue(
    rvalue: &mut Rvalue,
    const_env: &HashMap<LocalId, Constant>,
    rest_stmts: &[crate::ir::Statement],
    term: Option<&crate::ir::Terminator>,
    live_out: &std::collections::BTreeSet<LocalId>,
    move_invalidations: &mut Vec<LocalId>,
) -> usize {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            propagate_operand(op, const_env, rest_stmts, term, live_out, move_invalidations)
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            let mut count =
                propagate_operand(lhs, const_env, rest_stmts, term, live_out, move_invalidations);
            count +=
                propagate_operand(rhs, const_env, rest_stmts, term, live_out, move_invalidations);
            count
        }
        Rvalue::Aggregate(_, ops) => {
            let mut count = 0;
            for op in ops {
                count += propagate_operand(
                    op,
                    const_env,
                    rest_stmts,
                    term,
                    live_out,
                    move_invalidations,
                );
            }
            count
        }
        Rvalue::Ref { .. } | Rvalue::AddressOf(_) | Rvalue::Len(_) | Rvalue::Discriminant(_) => 0,
    }
}

fn propagate_operand(
    op: &mut Operand,
    const_env: &HashMap<LocalId, Constant>,
    rest_stmts: &[crate::ir::Statement],
    term: Option<&crate::ir::Terminator>,
    live_out: &std::collections::BTreeSet<LocalId>,
    move_invalidations: &mut Vec<LocalId>,
) -> usize {
    match op {
        Operand::Copy(place) if place.projection.is_empty() => {
            if let Some(c) = const_env.get(&place.local) {
                *op = Operand::Constant(c.clone());
                return 1;
            }
            0
        }
        Operand::Move(place) if place.projection.is_empty() => {
            let local = place.local;
            move_invalidations.push(local);
            if !live_out.contains(&local)
                && !is_local_referenced_later(local, rest_stmts, term)
                && let Some(c) = const_env.get(&local)
            {
                *op = Operand::Constant(c.clone());
                return 1;
            }
            0
        }
        _ => 0,
    }
}

fn is_local_referenced_later(
    local: LocalId,
    rest_stmts: &[crate::ir::Statement],
    term: Option<&crate::ir::Terminator>,
) -> bool {
    for stmt in rest_stmts {
        if stmt_references_local(&stmt.kind, local) {
            return true;
        }
    }
    if let Some(t) = term
        && term_references_local(&t.kind, local)
    {
        return true;
    }
    false
}

fn place_references_local(place: &Place, local: LocalId) -> bool {
    if place.local == local {
        return true;
    }
    place
        .projection
        .iter()
        .any(|p| matches!(p, ProjectionElem::Index(idx) if *idx == local))
}

fn operand_references_local(op: &Operand, local: LocalId) -> bool {
    match op {
        Operand::Copy(p) | Operand::Move(p) => place_references_local(p, local),
        Operand::Constant(_) => false,
    }
}

fn stmt_references_local(kind: &StatementKind, local: LocalId) -> bool {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            place_references_local(place, local) || rvalue_references_local(rvalue, local)
        }
        StatementKind::StorageLive(l) | StatementKind::StorageDead(l) => *l == local,
        StatementKind::SetDiscriminant { place, .. } | StatementKind::Drop(place) => {
            place_references_local(place, local)
        }
        StatementKind::Nop => false,
    }
}

fn rvalue_references_local(rvalue: &Rvalue, local: LocalId) -> bool {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(_, op, _) => {
            operand_references_local(op, local)
        }
        Rvalue::Ref { place, .. } | Rvalue::AddressOf(place) | Rvalue::Len(place)
        | Rvalue::Discriminant(place) => {
            place_references_local(place, local)
        }
        Rvalue::BinaryOp(_, l, r) => {
            operand_references_local(l, local) || operand_references_local(r, local)
        }
        Rvalue::Aggregate(_, ops) => ops.iter().any(|op| operand_references_local(op, local)),
    }
}

fn term_references_local(kind: &TerminatorKind, local: LocalId) -> bool {
    match kind {
        TerminatorKind::Return => local == LocalId::RETURN_PLACE,
        TerminatorKind::SwitchInt { discr, .. } => operand_references_local(discr, local),
        TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } => {
            operand_references_local(func, local)
                || args.iter().any(|a| operand_references_local(a, local))
                || place_references_local(destination, local)
        }
        TerminatorKind::Drop { place, .. } => place_references_local(place, local),
        TerminatorKind::Assert { cond, .. } => operand_references_local(cond, local),
        TerminatorKind::Goto { .. } | TerminatorKind::Unreachable => false,
    }
}

fn const_switch_key(c: &Constant) -> Option<u128> {
    match c.value {
        ConstValue::Bool(b) => Some(u128::from(b)),
        ConstValue::Char(ch) => Some(ch as u128),
        ConstValue::Int(v) if v >= 0 => Some(v as u128),
        _ => None,
    }
}

fn try_fold_rvalue(rvalue: &Rvalue) -> Option<Constant> {
    match rvalue {
        Rvalue::UnaryOp(op, Operand::Constant(c)) => fold_unary(*op, c),
        Rvalue::BinaryOp(op, Operand::Constant(lhs), Operand::Constant(rhs)) => {
            fold_binary(*op, lhs, rhs)
        }
        _ => None,
    }
}

fn fold_unary(op: UnOp, c: &Constant) -> Option<Constant> {
    match (op, &c.value, c.ty) {
        (UnOp::Not, ConstValue::Bool(b), TyKind::Bool) => {
            Some(Constant::new(TyKind::Bool, ConstValue::Bool(!b)))
        }
        (UnOp::Not, ConstValue::Int(v), TyKind::Int(ity)) => {
            let res = if ity.signed() {
                !v
            } else {
                (!v) & ((1i128 << ity.bits()) - 1)
            };
            Some(Constant::new(TyKind::Int(ity), ConstValue::Int(res)))
        }
        (UnOp::Neg, ConstValue::Int(v), TyKind::Int(ity)) if ity.signed() => {
            let neg = v.checked_neg()?;
            if !ity.contains(neg) {
                return None;
            }
            Some(Constant::new(TyKind::Int(ity), ConstValue::Int(neg)))
        }
        (UnOp::Neg, ConstValue::Float(f), TyKind::Float(fty)) => {
            let val = match fty {
                FloatTy::F32 => f64::from(-(f.to_f64() as f32)),
                FloatTy::F64 => -f.to_f64(),
            };
            Some(Constant::new(
                TyKind::Float(fty),
                ConstValue::Float(ConstFloat::from_f64(val)),
            ))
        }
        _ => None,
    }
}

fn fold_binary(op: BinOp, lhs: &Constant, rhs: &Constant) -> Option<Constant> {
    if lhs.ty != rhs.ty {
        return None;
    }
    match (&lhs.value, &rhs.value, lhs.ty) {
        (ConstValue::Int(a), ConstValue::Int(b), TyKind::Int(ity)) => {
            fold_int_binary(op, *a, *b, ity)
        }
        (ConstValue::Bool(a), ConstValue::Bool(b), TyKind::Bool) => {
            let res = match op {
                BinOp::BitAnd => *a & *b,
                BinOp::BitOr => *a | *b,
                BinOp::BitXor => *a ^ *b,
                BinOp::Eq => a == b,
                BinOp::Ne => a != b,
                BinOp::Lt => !*a & *b,
                BinOp::Le => a <= b,
                BinOp::Gt => *a & !*b,
                BinOp::Ge => a >= b,
                _ => return None,
            };
            Some(Constant::new(TyKind::Bool, ConstValue::Bool(res)))
        }
        (ConstValue::Char(a), ConstValue::Char(b), TyKind::Char) => {
            let res = match op {
                BinOp::Eq => a == b,
                BinOp::Ne => a != b,
                BinOp::Lt => a < b,
                BinOp::Le => a <= b,
                BinOp::Gt => a > b,
                BinOp::Ge => a >= b,
                _ => return None,
            };
            Some(Constant::new(TyKind::Bool, ConstValue::Bool(res)))
        }
        (ConstValue::Float(a), ConstValue::Float(b), TyKind::Float(fty)) => {
            fold_float_binary(op, a.to_f64(), b.to_f64(), fty)
        }
        _ => None,
    }
}

pub(crate) fn eval_int_binop(op: BinOp, a: i128, b: i128, ity: IntTy) -> Result<i128, &'static str> {
    let bits = ity.bits();
    let res = match op {
        BinOp::Add => a.checked_add(b).ok_or("integer addition overflow")?,
        BinOp::Sub => a.checked_sub(b).ok_or("integer subtraction overflow")?,
        BinOp::Mul => a.checked_mul(b).ok_or("integer multiplication overflow")?,
        BinOp::Div => {
            if b == 0 {
                return Err("division by zero");
            }
            if ity.signed() && a == ity.min_value() && b == -1 {
                return Err("integer division overflow");
            }
            a.checked_div(b).ok_or("integer division overflow")?
        }
        BinOp::Rem => {
            if b == 0 {
                return Err("remainder by zero");
            }
            if ity.signed() && a == ity.min_value() && b == -1 {
                return Err("integer remainder overflow");
            }
            a.checked_rem(b).ok_or("integer remainder overflow")?
        }
        BinOp::BitXor => a ^ b,
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::Shl => {
            if b < 0 || b >= i128::from(bits) {
                return Err("shift left overflow");
            }
            let shift = b as u32;
            if ity.signed() {
                let raw = (a as i64).wrapping_shl(shift);
                let unused = 64 - bits;
                i128::from((raw << unused) >> unused)
            } else {
                let mask = (1i128 << bits) - 1;
                (a << shift) & mask
            }
        }
        BinOp::Shr => {
            if b < 0 || b >= i128::from(bits) {
                return Err("shift right overflow");
            }
            let shift = b as u32;
            if ity.signed() {
                i128::from((a as i64) >> shift)
            } else {
                i128::from((a as u64) >> shift)
            }
        }
        BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Ne | BinOp::Ge | BinOp::Gt => {
            unreachable!("comparison handled separately")
        }
    };

    if !ity.contains(res) {
        return Err("integer arithmetic overflow");
    }
    Ok(res)
}

fn fold_int_binary(op: BinOp, a: i128, b: i128, ity: IntTy) -> Option<Constant> {
    if op.is_comparison() {
        let cmp = match op {
            BinOp::Eq => a == b,
            BinOp::Ne => a != b,
            BinOp::Lt => a < b,
            BinOp::Le => a <= b,
            BinOp::Gt => a > b,
            BinOp::Ge => a >= b,
            _ => unreachable!(),
        };
        return Some(Constant::new(TyKind::Bool, ConstValue::Bool(cmp)));
    }

    let res = eval_int_binop(op, a, b, ity).ok()?;
    Some(Constant::new(TyKind::Int(ity), ConstValue::Int(res)))
}

fn fold_float_binary(op: BinOp, a: f64, b: f64, fty: FloatTy) -> Option<Constant> {
    if op.is_comparison() {
        let cmp = match op {
            BinOp::Eq => a == b,
            BinOp::Ne => a != b,
            BinOp::Lt => a < b,
            BinOp::Le => a <= b,
            BinOp::Gt => a > b,
            BinOp::Ge => a >= b,
            _ => unreachable!(),
        };
        return Some(Constant::new(TyKind::Bool, ConstValue::Bool(cmp)));
    }

    let res = match fty {
        FloatTy::F32 => {
            let (fa, fb) = (a as f32, b as f32);
            let r = match op {
                BinOp::Add => fa + fb,
                BinOp::Sub => fa - fb,
                BinOp::Mul => fa * fb,
                BinOp::Div => fa / fb,
                BinOp::Rem => fa % fb,
                _ => return None,
            };
            f64::from(r)
        }
        FloatTy::F64 => match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => a / b,
            BinOp::Rem => a % b,
            _ => return None,
        },
    };

    Some(Constant::new(
        TyKind::Float(fty),
        ConstValue::Float(ConstFloat::from_f64(res)),
    ))
}
