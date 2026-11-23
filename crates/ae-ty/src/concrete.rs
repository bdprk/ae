//! The concrete type canonical form the compiler lowers to (P1.2).
//!
//! The variants carry the compiler's interned ids (structs, pointers,
//! signatures, shared boxes, closures, threads). The arenas still live in
//! the compiler's `CompileContext`; this module owns the shape and the pure
//! predicates so both the resolver and later crates can name a concrete
//! type without depending on the compiler crate.

/// The canonical concrete type. `Type` is the spelling the compiler uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TyKind {
    Int,
    Bool,
    F64,
    String,
    StrView,
    Vec,
    StrVec,
    VecF,
    VecRef,
    StrVecRef,
    VecFRef,
    Map,
    MapF,
    Array(usize),
    Struct(usize),
    StructRef(usize),
    StructMut(usize),
    Enum(&'static str, usize),
    Tokens,
    Closure(usize),
    Fn(usize),
    Ptr(usize),
    PtrMut(usize),
    Thread(usize),
    Atomic,
    Shared(usize),
    Mutex,
    Guard,
    Chan,
    Exec,
    TaskScope,
    Pool,
}

pub type Type = TyKind;

impl TyKind {
    /// The C layout of a machine scalar, when this type is one.
    pub fn scalar_c_layout(self) -> Option<(i64, i64)> {
        match self {
            TyKind::Int | TyKind::F64 => Some((8, 8)),
            TyKind::Bool => Some((1, 1)),
            _ => None,
        }
    }

    /// Owned types carry heap resources: they move, and every live slot is
    /// released on scope exit and control-flow jumps. Enums own their payload
    /// slots, which may hold strings. A raw pointer is a plain value: copying
    /// it moves nothing, so it neither moves nor releases.
    pub fn owned(self) -> bool {
        matches!(self, TyKind::String | TyKind::Vec | TyKind::StrVec | TyKind::VecF | TyKind::Map | TyKind::MapF | TyKind::Struct(_) | TyKind::Enum(..) | TyKind::Thread(_) | TyKind::Guard | TyKind::TaskScope)
    }
}