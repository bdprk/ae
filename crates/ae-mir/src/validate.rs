//! Structural and type validation for MIR [`Body`] graphs (P3.1).

use std::collections::VecDeque;
use std::fmt;

use ae_ty::{TyKind, TypeInterner, accepts_value};

use crate::ir::{
    AggregateKind, AssertMessage, BasicBlockId, Body, ConstValue, LocalId, Operand, Place,
    ProjectionElem, Rvalue, StatementKind, TerminatorKind,
};

/// Errors reported by [`validate`] and [`validate_with_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirValidationError {
    EmptyBody,
    InvalidArgCount {
        arg_count: usize,
        local_count: usize,
    },
    MissingReturnPlace {
        block: BasicBlockId,
    },
    MissingTerminator {
        block: BasicBlockId,
    },
    InvalidBasicBlock {
        from: BasicBlockId,
        target: BasicBlockId,
    },
    UnreachableBlock {
        block: BasicBlockId,
    },
    InvalidLocal {
        block: BasicBlockId,
        local: LocalId,
    },
    InvalidProjection {
        block: BasicBlockId,
        place: Place,
        reason: &'static str,
    },
    InvalidIndexType {
        block: BasicBlockId,
        local: LocalId,
        ty: TyKind,
    },
    TypeMismatch {
        block: BasicBlockId,
        expected: TyKind,
        found: TyKind,
    },
    InvalidConstantType {
        block: BasicBlockId,
        ty: TyKind,
    },
    InvalidDrop {
        block: BasicBlockId,
        ty: TyKind,
    },
    InvalidSwitchDiscriminant {
        block: BasicBlockId,
        ty: TyKind,
    },
}

impl fmt::Display for MirValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBody => f.write_str("MIR body has no basic blocks"),
            Self::InvalidArgCount {
                arg_count,
                local_count,
            } => write!(
                f,
                "arg_count ({arg_count}) exceeds available locals ({local_count})"
            ),
            Self::MissingReturnPlace { block } => {
                write!(f, "bb{}: return terminator with no _0 return local", block.0)
            }
            Self::MissingTerminator { block } => {
                write!(f, "bb{}: basic block has no terminator", block.0)
            }
            Self::InvalidBasicBlock { from, target } => {
                write!(f, "bb{}: successor bb{} is out of range", from.0, target.0)
            }
            Self::UnreachableBlock { block } => {
                write!(f, "bb{}: basic block is unreachable from bb0", block.0)
            }
            Self::InvalidLocal { block, local } => {
                write!(f, "bb{}: local _{} is out of range", block.0, local.0)
            }
            Self::InvalidProjection {
                block,
                place,
                reason,
            } => write!(f, "bb{}: invalid projection on {place}: {reason}", block.0),
            Self::InvalidIndexType { block, local, ty } => write!(
                f,
                "bb{}: index local _{} must be an integer type, found {ty:?}",
                block.0, local.0
            ),
            Self::TypeMismatch {
                block,
                expected,
                found,
            } => write!(
                f,
                "bb{}: type mismatch: expected {expected:?}, found {found:?}",
                block.0
            ),
            Self::InvalidConstantType { block, ty } => write!(
                f,
                "bb{}: constant value does not match declared type {ty:?}",
                block.0
            ),
            Self::InvalidDrop { block, ty } => {
                write!(f, "bb{}: cannot drop non-owned type {ty:?}", block.0)
            }
            Self::InvalidSwitchDiscriminant { block, ty } => write!(
                f,
                "bb{}: SwitchInt discriminant must be int, bool, or char, found {ty:?}",
                block.0
            ),
        }
    }
}

/// Validates structural CFG invariants, local bounds, projections, and basic type sanity.
pub fn validate(body: &Body) -> Result<(), Vec<MirValidationError>> {
    validate_with_types(body, None)
}

/// Validates a MIR [`Body`] with optional [`TypeInterner`] access for deeper projection checks.
pub fn validate_with_types(
    body: &Body,
    interner: Option<&TypeInterner>,
) -> Result<(), Vec<MirValidationError>> {
    let mut errors = Vec::new();

    if body.basic_blocks.is_empty() {
        errors.push(MirValidationError::EmptyBody);
    }

    if body.arg_count > 0 && body.arg_count + 1 > body.locals.len() {
        errors.push(MirValidationError::InvalidArgCount {
            arg_count: body.arg_count,
            local_count: body.locals.len(),
        });
    }

    for (idx, bb) in body.basic_blocks.iter().enumerate() {
        let block = BasicBlockId(idx as u32);

        for stmt in &bb.statements {
            validate_statement(body, interner, block, &stmt.kind, &mut errors);
        }

        match &bb.terminator {
            None => errors.push(MirValidationError::MissingTerminator { block }),
            Some(term) => validate_terminator(body, interner, block, &term.kind, &mut errors),
        }
    }

    if !body.basic_blocks.is_empty() {
        check_reachability(body, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_reachability(body: &Body, errors: &mut Vec<MirValidationError>) {
    let n = body.basic_blocks.len();
    let mut visited = vec![false; n];
    let mut queue = VecDeque::new();

    visited[0] = true;
    queue.push_back(BasicBlockId::ENTRY);

    while let Some(bb_id) = queue.pop_front() {
        if let Some(term) = &body.basic_blocks[bb_id.index()].terminator {
            for succ in term.kind.successors() {
                if succ.index() < n && !visited[succ.index()] {
                    visited[succ.index()] = true;
                    queue.push_back(succ);
                }
            }
        }
    }

    for (idx, &seen) in visited.iter().enumerate() {
        if !seen {
            errors.push(MirValidationError::UnreachableBlock {
                block: BasicBlockId(idx as u32),
            });
        }
    }
}

fn check_local(
    body: &Body,
    block: BasicBlockId,
    local: LocalId,
    errors: &mut Vec<MirValidationError>,
) -> Option<TyKind> {
    if let Some(decl) = body.local_decl(local) {
        Some(decl.ty)
    } else {
        errors.push(MirValidationError::InvalidLocal { block, local });
        None
    }
}

fn validate_place(
    body: &Body,
    interner: Option<&TypeInterner>,
    block: BasicBlockId,
    place: &Place,
    errors: &mut Vec<MirValidationError>,
) -> Option<TyKind> {
    let mut current_ty = check_local(body, block, place.local, errors);

    for elem in &place.projection {
        match elem {
            ProjectionElem::Deref => {
                current_ty = match current_ty {
                    Some(TyKind::Ptr(id) | TyKind::PtrMut(id)) => interner.and_then(|i| {
                        if id < i.pointee_count() {
                            Some(i.pointee(id))
                        } else {
                            errors.push(MirValidationError::InvalidProjection {
                                block,
                                place: place.clone(),
                                reason: "pointer pointee id out of range",
                            });
                            None
                        }
                    }),
                    Some(TyKind::Shared(_)) => None,
                    Some(TyKind::StructRef(id) | TyKind::StructMut(id)) => {
                        Some(TyKind::Struct(id))
                    }
                    Some(TyKind::VectorRef(id)) => Some(TyKind::Vector(id)),
                    Some(_) => {
                        errors.push(MirValidationError::InvalidProjection {
                            block,
                            place: place.clone(),
                            reason: "cannot dereference non-pointer type",
                        });
                        None
                    }
                    None => None,
                };
            }
            ProjectionElem::Index(idx_local) => {
                if let Some(idx_ty) = check_local(body, block, *idx_local, errors)
                    && !matches!(idx_ty, TyKind::Int(_))
                {
                    errors.push(MirValidationError::InvalidIndexType {
                        block,
                        local: *idx_local,
                        ty: idx_ty,
                    });
                }
                current_ty = match current_ty {
                    Some(TyKind::ArrayOf(id, _)) => interner.and_then(|i| {
                        if id < i.array_elem_count() {
                            Some(i.array_elem(id))
                        } else {
                            errors.push(MirValidationError::InvalidProjection {
                                block,
                                place: place.clone(),
                                reason: "array element id out of range",
                            });
                            None
                        }
                    }),
                    Some(TyKind::Slice(id) | TyKind::SliceMut(id)) => interner.and_then(|i| {
                        if id < i.slice_elem_count() {
                            Some(i.slice_elem(id))
                        } else {
                            errors.push(MirValidationError::InvalidProjection {
                                block,
                                place: place.clone(),
                                reason: "slice element id out of range",
                            });
                            None
                        }
                    }),
                    Some(TyKind::Vector(id) | TyKind::VectorRef(id)) => interner.and_then(|i| {
                        if id < i.vector_elem_count() {
                            Some(i.vector_elem(id))
                        } else {
                            errors.push(MirValidationError::InvalidProjection {
                                block,
                                place: place.clone(),
                                reason: "vector element id out of range",
                            });
                            None
                        }
                    }),
                    Some(_) => {
                        errors.push(MirValidationError::InvalidProjection {
                            block,
                            place: place.clone(),
                            reason: "cannot index non-indexable type",
                        });
                        None
                    }
                    None => None,
                };
            }
            ProjectionElem::Field(_) => {
                if let Some(ty) = current_ty
                    && (ty.scalar().is_some()
                        || matches!(
                            ty,
                            TyKind::Unit | TyKind::Ptr(_) | TyKind::PtrMut(_) | TyKind::StrView
                        ))
                {
                    errors.push(MirValidationError::InvalidProjection {
                        block,
                        place: place.clone(),
                        reason: "cannot project field on primitive type",
                    });
                }
                current_ty = None;
            }
            ProjectionElem::Downcast(_) => {
                if let Some(ty) = current_ty
                    && !matches!(ty, TyKind::Enum(..))
                {
                    errors.push(MirValidationError::InvalidProjection {
                        block,
                        place: place.clone(),
                        reason: "cannot downcast non-enum type",
                    });
                }
                current_ty = None;
            }
        }
    }

    current_ty
}

fn validate_operand(
    body: &Body,
    interner: Option<&TypeInterner>,
    block: BasicBlockId,
    op: &Operand,
    errors: &mut Vec<MirValidationError>,
) -> Option<TyKind> {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            validate_place(body, interner, block, place, errors)
        }
        Operand::Constant(c) => {
            let valid = matches!(
                (&c.value, c.ty),
                (ConstValue::Unit, TyKind::Unit)
                    | (ConstValue::Bool(_), TyKind::Bool)
                    | (ConstValue::Char(_), TyKind::Char)
                    | (ConstValue::Int(_), TyKind::Int(_))
                    | (ConstValue::Float(_), TyKind::Float(_))
                    | (ConstValue::String(_), TyKind::String | TyKind::StrView)
                    | (ConstValue::FnDef(_), TyKind::Fn(_) | TyKind::Closure(_))
            );
            if !valid {
                errors.push(MirValidationError::InvalidConstantType { block, ty: c.ty });
            }
            Some(c.ty)
        }
    }
}

fn validate_statement(
    body: &Body,
    interner: Option<&TypeInterner>,
    block: BasicBlockId,
    kind: &StatementKind,
    errors: &mut Vec<MirValidationError>,
) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            let lhs_ty = validate_place(body, interner, block, place, errors);
            let rhs_ty = validate_rvalue(body, interner, block, rvalue, lhs_ty, errors);
            if let (Some(expected), Some(found)) = (lhs_ty, rhs_ty)
                && !accepts_value(expected, found, None)
            {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    expected,
                    found,
                });
            }
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            check_local(body, block, *local, errors);
        }
        StatementKind::SetDiscriminant { place, .. } => {
            if let Some(ty) = validate_place(body, interner, block, place, errors)
                && !matches!(ty, TyKind::Enum(..))
            {
                errors.push(MirValidationError::InvalidProjection {
                    block,
                    place: place.clone(),
                    reason: "SetDiscriminant requires an enum place",
                });
            }
        }
        StatementKind::Drop(place) => {
            if let Some(ty) = validate_place(body, interner, block, place, errors)
                && !ty.owned()
            {
                errors.push(MirValidationError::InvalidDrop { block, ty });
            }
        }
        StatementKind::Nop => {}
    }
}

fn validate_rvalue(
    body: &Body,
    interner: Option<&TypeInterner>,
    block: BasicBlockId,
    rvalue: &Rvalue,
    lhs_ty: Option<TyKind>,
    errors: &mut Vec<MirValidationError>,
) -> Option<TyKind> {
    match rvalue {
        Rvalue::Use(op) => validate_operand(body, interner, block, op, errors),
        Rvalue::Ref { place, .. } | Rvalue::AddressOf(place) => {
            validate_place(body, interner, block, place, errors);
            None
        }
        Rvalue::BinaryOp(op, lhs, rhs) => {
            let lty = validate_operand(body, interner, block, lhs, errors);
            let rty = validate_operand(body, interner, block, rhs, errors);
            if op.is_comparison() {
                Some(TyKind::Bool)
            } else if let (Some(l), Some(r)) = (lty, rty)
                && l == r
            {
                Some(l)
            } else {
                None
            }
        }
        Rvalue::UnaryOp(_, op) => validate_operand(body, interner, block, op, errors),
        Rvalue::Cast(_, op, target_ty) => {
            validate_operand(body, interner, block, op, errors);
            Some(*target_ty)
        }
        Rvalue::Aggregate(kind, ops) => {
            for op in ops {
                validate_operand(body, interner, block, op, errors);
            }
            match kind {
                AggregateKind::Struct(id) => Some(TyKind::Struct(*id)),
                AggregateKind::Closure(id) => Some(TyKind::Closure(*id)),
                AggregateKind::Array(_elem_ty, elem_id) => Some(TyKind::ArrayOf(*elem_id, ops.len())),
                AggregateKind::Tuple | AggregateKind::Enum(..) => None,
            }
        }
        Rvalue::Len(place) | Rvalue::Discriminant(place) => {
            validate_place(body, interner, block, place, errors);
            if let Some(ty) = lhs_ty
                && !matches!(ty, TyKind::Int(_))
            {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    expected: TyKind::Int(ae_ty::IntTy::I64),
                    found: ty,
                });
            }
            None
        }
    }
}

fn validate_terminator(
    body: &Body,
    interner: Option<&TypeInterner>,
    block: BasicBlockId,
    kind: &TerminatorKind,
    errors: &mut Vec<MirValidationError>,
) {
    for succ in kind.successors() {
        if succ.index() >= body.basic_blocks.len() {
            errors.push(MirValidationError::InvalidBasicBlock {
                from: block,
                target: succ,
            });
        }
    }

    match kind {
        TerminatorKind::Goto { .. } | TerminatorKind::Unreachable => {}
        TerminatorKind::Return => {
            if body.locals.is_empty() {
                errors.push(MirValidationError::MissingReturnPlace { block });
            }
        }
        TerminatorKind::SwitchInt { discr, .. } => {
            if let Some(ty) = validate_operand(body, interner, block, discr, errors)
                && !matches!(ty, TyKind::Bool | TyKind::Int(_) | TyKind::Char)
            {
                errors.push(MirValidationError::InvalidSwitchDiscriminant { block, ty });
            }
        }
        TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } => {
            validate_operand(body, interner, block, func, errors);
            for arg in args {
                validate_operand(body, interner, block, arg, errors);
            }
            validate_place(body, interner, block, destination, errors);
        }
        TerminatorKind::Drop { place, .. } => {
            if let Some(ty) = validate_place(body, interner, block, place, errors)
                && !ty.owned()
            {
                errors.push(MirValidationError::InvalidDrop { block, ty });
            }
        }
        TerminatorKind::Assert { cond, msg, .. } => {
            if let Some(ty) = validate_operand(body, interner, block, cond, errors)
                && ty != TyKind::Bool
            {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    expected: TyKind::Bool,
                    found: ty,
                });
            }
            match msg {
                AssertMessage::BoundsCheck { len, index } => {
                    validate_operand(body, interner, block, len, errors);
                    validate_operand(body, interner, block, index, errors);
                }
                AssertMessage::DivisionByZero(op) | AssertMessage::RemainderByZero(op) => {
                    validate_operand(body, interner, block, op, errors);
                }
                AssertMessage::Overflow(_, lhs, rhs) => {
                    validate_operand(body, interner, block, lhs, errors);
                    validate_operand(body, interner, block, rhs, errors);
                }
                AssertMessage::Custom(_) => {}
            }
        }
    }
}
