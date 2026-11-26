//! Mid-level Intermediate Representation (MIR) for the AE compiler (P3.1).
//!
//! This crate defines a pure CFG-based MIR independent of syntax and HIR.
//! It depends only on `ae-span` and `ae-ty`.

pub mod fmt;
pub mod ir;
pub mod validate;

pub use fmt::{format_body, format_ty};
pub use ir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, BorrowKind, CastKind,
    ConstFloat, ConstValue, Constant, LocalDecl, LocalId, LocalKind, Mutability, Operand, Place,
    ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets, Terminator, TerminatorKind,
    UnOp,
};
pub use validate::{MirValidationError, validate, validate_with_types};
