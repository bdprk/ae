//! Canonical type rules shared by the resolver and the compiler (P1.2).
//!
//! The scalar/operator decisions live here so the editor-facing resolver
//! (whose `TypeSpelling` names may not identify a concrete generic
//! instance) and the compiler (which owns concrete layouts and instance
//! identities) cannot drift: both call the same rule and own only the
//! conversion into its three-scalar domain.
pub mod concrete;
pub mod op;

pub use concrete::{TyKind, Type};
pub use op::{infix, prefix, scalar_compound_assignment, valid_array_index, OperatorError, Scalar};
