//! Pure MIR reference evaluator / interpreter (P3.1/P3.2).
//!
//! Executes a validated MIR [`Body`] in isolation with explicit dynamic tracking
//! of per-local initialization and moved state, checked integer arithmetic,
//! assertion checks, and a configurable step-limit guard.

use std::fmt;

use ae_ty::{DefId, FloatTy, IntTy, TyKind};

use crate::ir::{
    AggregateKind, AssertMessage, BasicBlockId, BinOp, Body, CastKind, ConstValue, LocalId,
    Operand, Place, ProjectionElem, Rvalue, StatementKind, TerminatorKind, UnOp,
};
use crate::opt::eval_int_binop;
use crate::validate::{MirValidationError, validate};

/// Default maximum number of execution steps before [`MirEvalError::StepLimitExceeded`].
pub const DEFAULT_STEP_LIMIT: usize = 100_000;

/// Runtime value produced and consumed by the MIR reference evaluator.
#[derive(Clone, Debug, PartialEq)]
pub enum MirValue {
    Unit,
    Bool(bool),
    Char(char),
    Int(i128, IntTy),
    Float(f64, FloatTy),
    String(String),
    FnDef(DefId),
    Tuple(Vec<MirValue>),
    Array(Vec<MirValue>),
    Aggregate {
        kind: AggregateKind,
        fields: Vec<MirValue>,
    },
    Enum {
        def_id: usize,
        variant: usize,
        fields: Vec<MirValue>,
    },
}

impl MirValue {
    pub fn i8(v: i8) -> Self {
        Self::Int(i128::from(v), IntTy::I8)
    }

    pub fn i32(v: i32) -> Self {
        Self::Int(i128::from(v), IntTy::I32)
    }

    pub fn i64(v: i64) -> Self {
        Self::Int(i128::from(v), IntTy::I64)
    }

    pub fn u8(v: u8) -> Self {
        Self::Int(i128::from(v), IntTy::U8)
    }

    pub fn u32(v: u32) -> Self {
        Self::Int(i128::from(v), IntTy::U32)
    }

    pub fn u64(v: u64) -> Self {
        Self::Int(i128::from(v), IntTy::U64)
    }

    pub fn usize(v: usize) -> Self {
        Self::Int(v as i128, IntTy::Usize)
    }

    pub fn f32(v: f32) -> Self {
        Self::Float(f64::from(v), FloatTy::F32)
    }

    pub fn f64(v: f64) -> Self {
        Self::Float(v, FloatTy::F64)
    }
}

/// Structured runtime/trap errors reported by [`eval_body`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirEvalError {
    InvalidBody(Vec<MirValidationError>),
    ArgCountMismatch { expected: usize, found: usize },
    UninitializedRead { block: BasicBlockId, local: LocalId },
    UseAfterMove { block: BasicBlockId, local: LocalId },
    DivisionByZero { block: BasicBlockId },
    RemainderByZero { block: BasicBlockId },
    IntegerOverflow { block: BasicBlockId, detail: String },
    AssertionFailed { block: BasicBlockId, message: String },
    UnreachableExecuted { block: BasicBlockId },
    StepLimitExceeded { limit: usize },
    IndexOutOfBounds { block: BasicBlockId, index: i128, len: usize },
    InvalidProjection { block: BasicBlockId, detail: String },
    TypeMismatch { block: BasicBlockId, detail: String },
    UnsupportedFeature { block: BasicBlockId, detail: String },
}

impl fmt::Display for MirEvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBody(errs) => write!(f, "invalid MIR body ({} error(s))", errs.len()),
            Self::ArgCountMismatch { expected, found } => {
                write!(f, "argument count mismatch: expected {expected}, found {found}")
            }
            Self::UninitializedRead { block, local } => {
                write!(f, "bb{}: read of uninitialized local _{}", block.0, local.0)
            }
            Self::UseAfterMove { block, local } => {
                write!(f, "bb{}: use after move of local _{}", block.0, local.0)
            }
            Self::DivisionByZero { block } => write!(f, "bb{}: division by zero", block.0),
            Self::RemainderByZero { block } => write!(f, "bb{}: remainder by zero", block.0),
            Self::IntegerOverflow { block, detail } => {
                write!(f, "bb{}: integer overflow: {detail}", block.0)
            }
            Self::AssertionFailed { block, message } => {
                write!(f, "bb{}: assertion failed: {message}", block.0)
            }
            Self::UnreachableExecuted { block } => {
                write!(f, "bb{}: reached Unreachable terminator", block.0)
            }
            Self::StepLimitExceeded { limit } => {
                write!(f, "MIR evaluator exceeded step limit of {limit}")
            }
            Self::IndexOutOfBounds { block, index, len } => {
                write!(f, "bb{}: index {index} out of bounds for length {len}", block.0)
            }
            Self::InvalidProjection { block, detail } => {
                write!(f, "bb{}: invalid projection: {detail}", block.0)
            }
            Self::TypeMismatch { block, detail } => {
                write!(f, "bb{}: type mismatch: {detail}", block.0)
            }
            Self::UnsupportedFeature { block, detail } => {
                write!(f, "bb{}: unsupported MIR feature: {detail}", block.0)
            }
        }
    }
}

impl std::error::Error for MirEvalError {}

#[derive(Clone, Debug)]
enum LocalSlot {
    Uninit,
    Moved,
    Init(MirValue),
}

/// Evaluates `body` with `args` using [`DEFAULT_STEP_LIMIT`].
pub fn eval_body(body: &Body, args: &[MirValue]) -> Result<MirValue, MirEvalError> {
    eval_body_with_limit(body, args, DEFAULT_STEP_LIMIT)
}

/// Evaluates `body` with `args` and a custom execution `step_limit`.
pub fn eval_body_with_limit(
    body: &Body,
    args: &[MirValue],
    step_limit: usize,
) -> Result<MirValue, MirEvalError> {
    if let Err(errs) = validate(body) {
        return Err(MirEvalError::InvalidBody(errs));
    }

    if args.len() != body.arg_count {
        return Err(MirEvalError::ArgCountMismatch {
            expected: body.arg_count,
            found: args.len(),
        });
    }

    let mut slots = vec![LocalSlot::Uninit; body.locals.len()];
    for (i, arg) in args.iter().enumerate() {
        slots[i + 1] = LocalSlot::Init(arg.clone());
    }

    let mut current_bb = BasicBlockId::ENTRY;
    let mut steps = 0usize;

    loop {
        let bb = &body.basic_blocks[current_bb.index()];

        for stmt in &bb.statements {
            steps += 1;
            if steps > step_limit {
                return Err(MirEvalError::StepLimitExceeded { limit: step_limit });
            }
            exec_statement(current_bb, &stmt.kind, &mut slots)?;
        }

        steps += 1;
        if steps > step_limit {
            return Err(MirEvalError::StepLimitExceeded { limit: step_limit });
        }

        let term = bb
            .terminator
            .as_ref()
            .expect("validated body always has terminator");

        match &term.kind {
            TerminatorKind::Goto { target } => {
                current_bb = *target;
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let val = eval_operand(current_bb, discr, &mut slots)?;
                let key = value_to_switch_key(current_bb, &val)?;
                current_bb = targets
                    .cases
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, t)| *t)
                    .unwrap_or(targets.otherwise);
            }
            TerminatorKind::Assert {
                cond,
                expected,
                msg,
                target,
            } => {
                let val = eval_operand(current_bb, cond, &mut slots)?;
                let MirValue::Bool(actual) = val else {
                    return Err(MirEvalError::TypeMismatch {
                        block: current_bb,
                        detail: format!("Assert condition must be Bool, found {val:?}"),
                    });
                };
                if actual != *expected {
                    let message = format_assert_message(msg);
                    return Err(MirEvalError::AssertionFailed {
                        block: current_bb,
                        message,
                    });
                }
                current_bb = *target;
            }
            TerminatorKind::Drop { place, target, .. } => {
                drop_place(current_bb, place, &mut slots)?;
                current_bb = *target;
            }
            TerminatorKind::Return => {
                return read_place(current_bb, &Place::from_local(LocalId::RETURN_PLACE), &slots);
            }
            TerminatorKind::Unreachable => {
                return Err(MirEvalError::UnreachableExecuted { block: current_bb });
            }
            TerminatorKind::Call { .. } => {
                return Err(MirEvalError::UnsupportedFeature {
                    block: current_bb,
                    detail: "TerminatorKind::Call is not supported in single-body pure MIR eval"
                        .to_string(),
                });
            }
        }
    }
}

fn format_assert_message(msg: &AssertMessage) -> String {
    match msg {
        AssertMessage::BoundsCheck { .. } => "index out of bounds".to_string(),
        AssertMessage::DivisionByZero(_) => "attempt to divide by zero".to_string(),
        AssertMessage::RemainderByZero(_) => "attempt to calculate remainder with zero".to_string(),
        AssertMessage::Overflow(op, _, _) => format!("attempt to compute {op:?} with overflow"),
        AssertMessage::Custom(s) => s.clone(),
    }
}

fn exec_statement(
    block: BasicBlockId,
    kind: &StatementKind,
    slots: &mut [LocalSlot],
) -> Result<(), MirEvalError> {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            let val = eval_rvalue(block, rvalue, slots)?;
            write_place(block, place, val, slots)
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            slots[local.index()] = LocalSlot::Uninit;
            Ok(())
        }
        StatementKind::SetDiscriminant {
            place,
            variant_index,
        } => {
            let slot = &mut slots[place.local.index()];
            match slot {
                LocalSlot::Uninit => Err(MirEvalError::UninitializedRead {
                    block,
                    local: place.local,
                }),
                LocalSlot::Moved => Err(MirEvalError::UseAfterMove {
                    block,
                    local: place.local,
                }),
                LocalSlot::Init(val) => {
                    if let MirValue::Enum { variant, .. } = val {
                        *variant = *variant_index;
                        Ok(())
                    } else {
                        Err(MirEvalError::TypeMismatch {
                            block,
                            detail: "SetDiscriminant target is not an Enum".to_string(),
                        })
                    }
                }
            }
        }
        StatementKind::Drop(place) => drop_place(block, place, slots),
        StatementKind::Nop => Ok(()),
    }
}

fn drop_place(
    block: BasicBlockId,
    place: &Place,
    slots: &mut [LocalSlot],
) -> Result<(), MirEvalError> {
    if place.projection.is_empty() {
        slots[place.local.index()] = LocalSlot::Uninit;
        Ok(())
    } else {
        Err(MirEvalError::InvalidProjection {
            block,
            detail: "dropping a projected place is not supported".to_string(),
        })
    }
}

fn read_place(
    block: BasicBlockId,
    place: &Place,
    slots: &[LocalSlot],
) -> Result<MirValue, MirEvalError> {
    let mut cur = match &slots[place.local.index()] {
        LocalSlot::Uninit => {
            return Err(MirEvalError::UninitializedRead {
                block,
                local: place.local,
            });
        }
        LocalSlot::Moved => {
            return Err(MirEvalError::UseAfterMove {
                block,
                local: place.local,
            });
        }
        LocalSlot::Init(val) => val,
    };

    for elem in &place.projection {
        match elem {
            ProjectionElem::Field(idx) => {
                cur = match cur {
                    MirValue::Tuple(fields)
                    | MirValue::Array(fields)
                    | MirValue::Aggregate { fields, .. }
                    | MirValue::Enum { fields, .. } => fields.get(*idx).ok_or_else(|| {
                        MirEvalError::InvalidProjection {
                            block,
                            detail: format!("field index {idx} out of bounds"),
                        }
                    })?,
                    _ => {
                        return Err(MirEvalError::InvalidProjection {
                            block,
                            detail: format!("cannot project field {idx} on {cur:?}"),
                        });
                    }
                };
            }
            ProjectionElem::Index(idx_local) => {
                let idx_val = read_place(block, &Place::from_local(*idx_local), slots)?;
                let MirValue::Int(idx, _) = idx_val else {
                    return Err(MirEvalError::TypeMismatch {
                        block,
                        detail: format!("index local _{} must be Int, found {idx_val:?}", idx_local.0),
                    });
                };
                let MirValue::Array(elems) = cur else {
                    return Err(MirEvalError::InvalidProjection {
                        block,
                        detail: format!("cannot index non-array value {cur:?}"),
                    });
                };
                if idx < 0 || (idx as usize) >= elems.len() {
                    return Err(MirEvalError::IndexOutOfBounds {
                        block,
                        index: idx,
                        len: elems.len(),
                    });
                }
                cur = &elems[idx as usize];
            }
            ProjectionElem::Downcast(expected_variant) => {
                if let MirValue::Enum { variant, .. } = cur {
                    if variant != expected_variant {
                        return Err(MirEvalError::InvalidProjection {
                            block,
                            detail: format!(
                                "downcast expected variant {expected_variant}, found {variant}"
                            ),
                        });
                    }
                } else {
                    return Err(MirEvalError::InvalidProjection {
                        block,
                        detail: "cannot downcast non-enum value".to_string(),
                    });
                }
            }
            ProjectionElem::Deref => {
                return Err(MirEvalError::UnsupportedFeature {
                    block,
                    detail: "ProjectionElem::Deref is not supported in pure MIR eval".to_string(),
                });
            }
        }
    }

    Ok(cur.clone())
}

fn write_place(
    block: BasicBlockId,
    place: &Place,
    value: MirValue,
    slots: &mut [LocalSlot],
) -> Result<(), MirEvalError> {
    if place.projection.is_empty() {
        slots[place.local.index()] = LocalSlot::Init(value);
        return Ok(());
    }

    // Resolve any index locals before mutably borrowing `slots`.
    let mut resolved_indices = Vec::new();
    for elem in &place.projection {
        if let ProjectionElem::Index(idx_local) = elem {
            let idx_val = read_place(block, &Place::from_local(*idx_local), slots)?;
            let MirValue::Int(idx, _) = idx_val else {
                return Err(MirEvalError::TypeMismatch {
                    block,
                    detail: format!("index local _{} must be Int", idx_local.0),
                });
            };
            resolved_indices.push(idx);
        }
    }
    let mut idx_iter = resolved_indices.into_iter();

    let mut cur = match &mut slots[place.local.index()] {
        LocalSlot::Uninit => {
            return Err(MirEvalError::UninitializedRead {
                block,
                local: place.local,
            });
        }
        LocalSlot::Moved => {
            return Err(MirEvalError::UseAfterMove {
                block,
                local: place.local,
            });
        }
        LocalSlot::Init(val) => val,
    };

    for elem in &place.projection {
        match elem {
            ProjectionElem::Field(idx) => {
                cur = match cur {
                    MirValue::Tuple(fields)
                    | MirValue::Array(fields)
                    | MirValue::Aggregate { fields, .. }
                    | MirValue::Enum { fields, .. } => fields.get_mut(*idx).ok_or_else(|| {
                        MirEvalError::InvalidProjection {
                            block,
                            detail: format!("field index {idx} out of bounds"),
                        }
                    })?,
                    _ => {
                        return Err(MirEvalError::InvalidProjection {
                            block,
                            detail: "cannot project field on primitive value".to_string(),
                        });
                    }
                };
            }
            ProjectionElem::Index(_) => {
                let idx = idx_iter.next().expect("resolved index");
                let MirValue::Array(elems) = cur else {
                    return Err(MirEvalError::InvalidProjection {
                        block,
                        detail: "cannot index non-array value".to_string(),
                    });
                };
                let len = elems.len();
                if idx < 0 || (idx as usize) >= len {
                    return Err(MirEvalError::IndexOutOfBounds {
                        block,
                        index: idx,
                        len,
                    });
                }
                cur = &mut elems[idx as usize];
            }
            ProjectionElem::Downcast(expected_variant) => {
                if let MirValue::Enum { variant, .. } = cur {
                    if *variant != *expected_variant {
                        return Err(MirEvalError::InvalidProjection {
                            block,
                            detail: format!(
                                "downcast expected variant {expected_variant}, found {variant}"
                            ),
                        });
                    }
                } else {
                    return Err(MirEvalError::InvalidProjection {
                        block,
                        detail: "cannot downcast non-enum value".to_string(),
                    });
                }
            }
            ProjectionElem::Deref => {
                return Err(MirEvalError::UnsupportedFeature {
                    block,
                    detail: "ProjectionElem::Deref is not supported in pure MIR eval".to_string(),
                });
            }
        }
    }

    *cur = value;
    Ok(())
}

fn eval_operand(
    block: BasicBlockId,
    op: &Operand,
    slots: &mut [LocalSlot],
) -> Result<MirValue, MirEvalError> {
    match op {
        Operand::Copy(place) => read_place(block, place, slots),
        Operand::Move(place) => {
            let val = read_place(block, place, slots)?;
            if place.projection.is_empty() {
                slots[place.local.index()] = LocalSlot::Moved;
            }
            Ok(val)
        }
        Operand::Constant(c) => Ok(eval_constant(block, &c.value, c.ty)?),
    }
}

fn eval_constant(
    block: BasicBlockId,
    value: &ConstValue,
    ty: TyKind,
) -> Result<MirValue, MirEvalError> {
    match (value, ty) {
        (ConstValue::Unit, TyKind::Unit) => Ok(MirValue::Unit),
        (ConstValue::Bool(b), TyKind::Bool) => Ok(MirValue::Bool(*b)),
        (ConstValue::Char(ch), TyKind::Char) => Ok(MirValue::Char(*ch)),
        (ConstValue::Int(v), TyKind::Int(ity)) => Ok(MirValue::Int(*v, ity)),
        (ConstValue::Float(f), TyKind::Float(fty)) => Ok(MirValue::Float(f.to_f64(), fty)),
        (ConstValue::String(s), TyKind::String | TyKind::StrView) => {
            Ok(MirValue::String(s.clone()))
        }
        (ConstValue::FnDef(def), TyKind::Fn(_) | TyKind::Closure(_)) => Ok(MirValue::FnDef(*def)),
        _ => Err(MirEvalError::TypeMismatch {
            block,
            detail: format!("constant {value:?} incompatible with type {ty:?}"),
        }),
    }
}

fn eval_rvalue(
    block: BasicBlockId,
    rvalue: &Rvalue,
    slots: &mut [LocalSlot],
) -> Result<MirValue, MirEvalError> {
    match rvalue {
        Rvalue::Use(op) => eval_operand(block, op, slots),
        Rvalue::UnaryOp(op, operand) => {
            let val = eval_operand(block, operand, slots)?;
            eval_unary(block, *op, val)
        }
        Rvalue::BinaryOp(op, lhs, rhs) => {
            let lval = eval_operand(block, lhs, slots)?;
            let rval = eval_operand(block, rhs, slots)?;
            eval_binary(block, *op, lval, rval)
        }
        Rvalue::Cast(kind, operand, target_ty) => {
            let val = eval_operand(block, operand, slots)?;
            eval_cast(block, *kind, val, *target_ty)
        }
        Rvalue::Aggregate(kind, ops) => {
            let mut fields = Vec::with_capacity(ops.len());
            for op in ops {
                fields.push(eval_operand(block, op, slots)?);
            }
            Ok(match kind {
                AggregateKind::Tuple => MirValue::Tuple(fields),
                AggregateKind::Array(_) => MirValue::Array(fields),
                AggregateKind::Enum(def_id, variant) => MirValue::Enum {
                    def_id: *def_id,
                    variant: *variant,
                    fields,
                },
                AggregateKind::Struct(_) | AggregateKind::Closure(_) => MirValue::Aggregate {
                    kind: *kind,
                    fields,
                },
            })
        }
        Rvalue::Len(place) => {
            let val = read_place(block, place, slots)?;
            match val {
                MirValue::Array(elems) => Ok(MirValue::usize(elems.len())),
                MirValue::String(s) => Ok(MirValue::usize(s.len())),
                _ => Err(MirEvalError::TypeMismatch {
                    block,
                    detail: format!("Rvalue::Len requires array or string, found {val:?}"),
                }),
            }
        }
        Rvalue::Discriminant(place) => {
            let val = read_place(block, place, slots)?;
            match val {
                MirValue::Enum { variant, .. } => Ok(MirValue::Int(variant as i128, IntTy::I64)),
                _ => Err(MirEvalError::TypeMismatch {
                    block,
                    detail: format!("Rvalue::Discriminant requires enum, found {val:?}"),
                }),
            }
        }
        Rvalue::Ref { .. } => Err(MirEvalError::UnsupportedFeature {
            block,
            detail: "Rvalue::Ref is not supported in pure MIR eval".to_string(),
        }),
    }
}

fn eval_unary(block: BasicBlockId, op: UnOp, val: MirValue) -> Result<MirValue, MirEvalError> {
    match (op, val) {
        (UnOp::Not, MirValue::Bool(b)) => Ok(MirValue::Bool(!b)),
        (UnOp::Not, MirValue::Int(v, ity)) => {
            let res = if ity.signed() {
                !v
            } else {
                (!v) & ((1i128 << ity.bits()) - 1)
            };
            Ok(MirValue::Int(res, ity))
        }
        (UnOp::Neg, MirValue::Int(v, ity)) if ity.signed() => {
            let neg = v.checked_neg().filter(|n| ity.contains(*n)).ok_or_else(|| {
                MirEvalError::IntegerOverflow {
                    block,
                    detail: format!("negation overflow on {v}"),
                }
            })?;
            Ok(MirValue::Int(neg, ity))
        }
        (UnOp::Neg, MirValue::Float(f, fty)) => {
            let res = match fty {
                FloatTy::F32 => f64::from(-(f as f32)),
                FloatTy::F64 => -f,
            };
            Ok(MirValue::Float(res, fty))
        }
        (op, other) => Err(MirEvalError::TypeMismatch {
            block,
            detail: format!("invalid unary op {op:?} for {other:?}"),
        }),
    }
}

fn eval_binary(
    block: BasicBlockId,
    op: BinOp,
    lhs: MirValue,
    rhs: MirValue,
) -> Result<MirValue, MirEvalError> {
    match (lhs, rhs) {
        (MirValue::Int(a, ity), MirValue::Int(b, rty)) if ity == rty => {
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
                return Ok(MirValue::Bool(cmp));
            }
            if op == BinOp::Div && b == 0 {
                return Err(MirEvalError::DivisionByZero { block });
            }
            if op == BinOp::Rem && b == 0 {
                return Err(MirEvalError::RemainderByZero { block });
            }
            let res = eval_int_binop(op, a, b, ity).map_err(|detail| {
                MirEvalError::IntegerOverflow {
                    block,
                    detail: detail.to_string(),
                }
            })?;
            Ok(MirValue::Int(res, ity))
        }
        (MirValue::Bool(a), MirValue::Bool(b)) => {
            let res = match op {
                BinOp::BitAnd => a & b,
                BinOp::BitOr => a | b,
                BinOp::BitXor => a ^ b,
                BinOp::Eq => a == b,
                BinOp::Ne => a != b,
                BinOp::Lt => !a & b,
                BinOp::Le => a <= b,
                BinOp::Gt => a & !b,
                BinOp::Ge => a >= b,
                _ => {
                    return Err(MirEvalError::TypeMismatch {
                        block,
                        detail: format!("invalid binary op {op:?} on Bool"),
                    });
                }
            };
            Ok(MirValue::Bool(res))
        }
        (MirValue::Char(a), MirValue::Char(b)) => {
            let res = match op {
                BinOp::Eq => a == b,
                BinOp::Ne => a != b,
                BinOp::Lt => a < b,
                BinOp::Le => a <= b,
                BinOp::Gt => a > b,
                BinOp::Ge => a >= b,
                _ => {
                    return Err(MirEvalError::TypeMismatch {
                        block,
                        detail: format!("invalid binary op {op:?} on Char"),
                    });
                }
            };
            Ok(MirValue::Bool(res))
        }
        (MirValue::Float(a, fty), MirValue::Float(b, rty)) if fty == rty => {
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
                return Ok(MirValue::Bool(cmp));
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
                        _ => {
                            return Err(MirEvalError::TypeMismatch {
                                block,
                                detail: format!("invalid binary op {op:?} on Float"),
                            });
                        }
                    };
                    f64::from(r)
                }
                FloatTy::F64 => match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    _ => {
                        return Err(MirEvalError::TypeMismatch {
                            block,
                            detail: format!("invalid binary op {op:?} on Float"),
                        });
                    }
                },
            };
            Ok(MirValue::Float(res, fty))
        }
        (l, r) => Err(MirEvalError::TypeMismatch {
            block,
            detail: format!("incompatible operands for {op:?}: {l:?} and {r:?}"),
        }),
    }
}

fn eval_cast(
    block: BasicBlockId,
    kind: CastKind,
    val: MirValue,
    target_ty: TyKind,
) -> Result<MirValue, MirEvalError> {
    match (kind, val, target_ty) {
        (CastKind::IntToInt, MirValue::Int(v, _), TyKind::Int(target)) => {
            let bits = target.bits();
            let casted = if target.signed() {
                let unused = 64 - bits;
                i128::from(((v as i64) << unused) >> unused)
            } else {
                let mask = (1i128 << bits) - 1;
                v & mask
            };
            Ok(MirValue::Int(casted, target))
        }
        (CastKind::IntToFloat, MirValue::Int(v, _), TyKind::Float(target)) => {
            let f = match target {
                FloatTy::F32 => f64::from(v as f32),
                FloatTy::F64 => v as f64,
            };
            Ok(MirValue::Float(f, target))
        }
        (CastKind::FloatToInt, MirValue::Float(f, _), TyKind::Int(target)) => {
            let bits = target.bits();
            let casted = if target.signed() {
                let raw = f as i64;
                let unused = 64 - bits;
                i128::from((raw << unused) >> unused)
            } else {
                let raw = f as u64;
                let mask = (1i128 << bits) - 1;
                i128::from(raw) & mask
            };
            Ok(MirValue::Int(casted, target))
        }
        (CastKind::FloatToFloat, MirValue::Float(f, _), TyKind::Float(target)) => {
            let casted = match target {
                FloatTy::F32 => f64::from(f as f32),
                FloatTy::F64 => f,
            };
            Ok(MirValue::Float(casted, target))
        }
        (CastKind::Misc, MirValue::Bool(b), TyKind::Int(target)) => {
            Ok(MirValue::Int(i128::from(b), target))
        }
        (CastKind::Misc, MirValue::Char(ch), TyKind::Int(target)) => {
            Ok(MirValue::Int(i128::from(ch as u32), target))
        }
        (CastKind::Misc, MirValue::Int(v, _), TyKind::Char) => {
            let ch = u32::try_from(v)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| MirEvalError::TypeMismatch {
                    block,
                    detail: format!("invalid char codepoint {v}"),
                })?;
            Ok(MirValue::Char(ch))
        }
        (kind, other, ty) => Err(MirEvalError::UnsupportedFeature {
            block,
            detail: format!("unsupported cast {kind:?} from {other:?} to {ty:?}"),
        }),
    }
}

fn value_to_switch_key(block: BasicBlockId, val: &MirValue) -> Result<u128, MirEvalError> {
    match val {
        MirValue::Bool(b) => Ok(u128::from(*b)),
        MirValue::Char(ch) => Ok(*ch as u128),
        MirValue::Int(v, _) if *v >= 0 => Ok(*v as u128),
        MirValue::Int(v, _) => Ok((*v as u64) as u128),
        other => Err(MirEvalError::TypeMismatch {
            block,
            detail: format!("SwitchInt discriminant must be Bool/Char/Int, found {other:?}"),
        }),
    }
}
