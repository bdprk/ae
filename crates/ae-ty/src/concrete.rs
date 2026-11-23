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

impl TyKind {
    /// The three-value scalar domain the operator rules work in. An absent
    /// scalar is a known non-scalar type.
    pub fn scalar(self) -> Option<crate::op::Scalar> {
        use crate::op::Scalar;
        match self {
            TyKind::Int => Some(Scalar::Int),
            TyKind::F64 => Some(Scalar::Float),
            TyKind::Bool => Some(Scalar::Bool),
            _ => None,
        }
    }
}

/// The canonical value-compatibility rule: a checked scalar fact decides the
/// question (the operand is that scalar type), and otherwise the concrete
/// types must be identical. Borrows do not coerce and owned values do not
/// convert implicitly.
pub fn accepts_value(expected: TyKind, actual: TyKind, checked: Option<crate::op::Scalar>) -> bool {
    match checked {
        Some(scalar) => expected.scalar() == Some(scalar),
        None => expected == actual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Scalar;

    #[test]
    fn scalar_bridge_names_the_three_scalars_and_nothing_else() {
        assert_eq!(TyKind::Int.scalar(), Some(Scalar::Int));
        assert_eq!(TyKind::F64.scalar(), Some(Scalar::Float));
        assert_eq!(TyKind::Bool.scalar(), Some(Scalar::Bool));
        assert_eq!(TyKind::String.scalar(), None);
        assert_eq!(TyKind::Ptr(3).scalar(), None);
        assert_eq!(Scalar::Int.ty(), TyKind::Int);
        assert_eq!(Scalar::Float.ty(), TyKind::F64);
        assert_eq!(Scalar::Bool.ty(), TyKind::Bool);
    }

    #[test]
    fn value_acceptance_prefers_the_checked_fact_and_never_coerces() {
        assert!(accepts_value(TyKind::Int, TyKind::Bool, Some(Scalar::Int)));
        assert!(!accepts_value(TyKind::Int, TyKind::Int, Some(Scalar::Bool)));
        assert!(accepts_value(TyKind::Int, TyKind::Int, None));
        assert!(!accepts_value(TyKind::Int, TyKind::Bool, None));
        assert!(accepts_value(TyKind::String, TyKind::String, None));
        assert!(!accepts_value(TyKind::String, TyKind::StrView, None));
        assert!(!accepts_value(TyKind::Vec, TyKind::StrVec, None));
    }
}
