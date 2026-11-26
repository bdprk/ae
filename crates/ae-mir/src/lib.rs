//! Mid-level Intermediate Representation (MIR) for the AE compiler (P3.1).
//!
//! This crate defines a pure CFG-based MIR independent of syntax and HIR.
//! It depends only on `ae-span` and `ae-ty`.

pub mod analysis;
pub mod borrowck;
pub mod eval;
pub mod fmt;
pub mod ir;
pub mod opt;
pub mod validate;

pub use analysis::{Cfg, InitAnalysis, InitState, LivenessAnalysis};
pub use borrowck::{BorrowError, BorrowErrorKind, Loan, Location, check_body};
pub use eval::{DEFAULT_STEP_LIMIT, MirEvalError, MirValue, eval_body, eval_body_with_limit};
pub use fmt::{format_body, format_ty};
pub use ir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, BorrowKind, CastKind,
    ConstFloat, ConstValue, Constant, LocalDecl, LocalId, LocalKind, Mutability, Operand, Place,
    ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets, Terminator, TerminatorKind,
    UnOp,
};
pub use opt::{
    OptStats, compress_goto_chains, fold_constants_and_propagate, optimize_body,
    prune_unreachable_blocks,
};
pub use validate::{MirValidationError, validate, validate_with_types};
