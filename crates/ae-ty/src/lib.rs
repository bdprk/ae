//! Canonical type rules shared by the resolver and the compiler (P1.2).
//!
//! The scalar/operator decisions live here so the editor-facing resolver
//! (whose `TypeSpelling` names may not identify a concrete generic
//! instance) and the compiler (which owns concrete layouts and instance
//! identities) cannot drift: both call the same rule and own only the
//! conversion into its three-scalar domain.
pub mod concrete;
pub mod id;
pub mod interner;
pub mod op;
pub mod spelling;
pub mod unify;

pub use concrete::{accepts_value, TyKind, Type};
pub use id::DefId;
pub use spelling::TypeSpelling;
pub use unify::{unify, Substitution, Term, TypeVar, UnifyError};
pub use interner::TypeInterner;
pub use op::{infix, prefix, scalar_compound_assignment, valid_array_index, OperatorError, Scalar};
