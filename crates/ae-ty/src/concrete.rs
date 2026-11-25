//! The concrete type canonical form the compiler lowers to (P1.2).
//!
//! The variants carry the compiler's interned ids (structs, pointers,
//! signatures, shared boxes, closures, threads). The arenas still live in
//! the compiler's `CompileContext`; this module owns the shape and the pure
//! predicates so both the resolver and later crates can name a concrete
//! type without depending on the compiler crate.

use crate::numeric::{FloatTy, IntTy};

/// The canonical concrete type. `Type` is the spelling the compiler uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TyKind {
    Int(IntTy),
    Bool,
    Float(FloatTy),
    Char,
    Unit,
    String,
    StrView,
    /// A generic vector whose element type is interned (ADR-0235).
    Vector(usize),
    /// A read-only borrow of a generic vector.
    VectorRef(usize),
    /// A generic map whose `(key, value)` types are interned (ADR-0235).
    MapOf(usize),
    /// A fixed-size borrowed array whose `(elem_id, len)` are interned/stored (ADR-0235).
    ArrayOf(usize, usize),
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
            TyKind::Int(width) => Some(((width.bits() / 8) as i64, (width.bits() / 8) as i64)),
            TyKind::Float(FloatTy::F64) => Some((8, 8)),
            TyKind::Float(FloatTy::F32) => Some((4, 4)),
            TyKind::Char => Some((4, 4)),
            TyKind::Bool => Some((1, 1)),
            _ => None,
        }
    }

    /// Owned types carry heap resources: they move, and every live slot is
    /// released on scope exit and control-flow jumps. Enums own their payload
    /// slots, which may hold strings. A raw pointer is a plain value: copying
    /// it moves nothing, so it neither moves nor releases.
    pub fn owned(self) -> bool {
        matches!(self, TyKind::String | TyKind::Vector(_) | TyKind::MapOf(_) | TyKind::Struct(_) | TyKind::Enum(..) | TyKind::Thread(_) | TyKind::Guard | TyKind::TaskScope)
    }
}

impl TyKind {
    /// The scalar domain the operator rules work in. An absent scalar is a
    /// known non-scalar type.
    pub fn scalar(self) -> Option<crate::op::Scalar> {
        use crate::op::Scalar;
        match self {
            TyKind::Int(width) => Some(Scalar::Int(width)),
            TyKind::Float(width) => Some(Scalar::Float(width)),
            TyKind::Bool => Some(Scalar::Bool),
            TyKind::Char => Some(Scalar::Char),
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
        assert_eq!(TyKind::Int(IntTy::I64).scalar(), Some(Scalar::Int(IntTy::I64)));
        assert_eq!(TyKind::Int(IntTy::I8).scalar(), Some(Scalar::Int(IntTy::I8)));
        assert_eq!(TyKind::Int(IntTy::U8).scalar(), Some(Scalar::Int(IntTy::U8)));
        assert_eq!(TyKind::Float(FloatTy::F64).scalar(), Some(Scalar::Float(FloatTy::F64)));
        assert_eq!(TyKind::Bool.scalar(), Some(Scalar::Bool));
        assert_eq!(TyKind::String.scalar(), None);
        assert_eq!(TyKind::Ptr(3).scalar(), None);
        assert_eq!(Scalar::Int(IntTy::I64).ty(), TyKind::Int(IntTy::I64));
        assert_eq!(Scalar::Int(IntTy::I8).ty(), TyKind::Int(IntTy::I8));
        assert_eq!(Scalar::Int(IntTy::U8).ty(), TyKind::Int(IntTy::U8));
        assert_eq!(Scalar::Float(FloatTy::F64).ty(), TyKind::Float(FloatTy::F64));
        assert_eq!(Scalar::Bool.ty(), TyKind::Bool);
    }

    #[test]
    fn value_acceptance_prefers_the_checked_fact_and_never_coerces() {
        assert!(accepts_value(TyKind::Int(IntTy::I64), TyKind::Bool, Some(Scalar::Int(IntTy::I64))));
        assert!(!accepts_value(TyKind::Int(IntTy::I64), TyKind::Int(IntTy::I64), Some(Scalar::Bool)));
        assert!(!accepts_value(TyKind::Int(IntTy::I8), TyKind::Int(IntTy::I64), Some(Scalar::Int(IntTy::I64))));
        assert!(accepts_value(TyKind::Int(IntTy::I64), TyKind::Int(IntTy::I64), None));
        assert!(!accepts_value(TyKind::Int(IntTy::I64), TyKind::Bool, None));
        assert!(accepts_value(TyKind::String, TyKind::String, None));
        assert!(!accepts_value(TyKind::String, TyKind::StrView, None));
        assert!(!accepts_value(TyKind::Vector(0), TyKind::Vector(1), None));
    }
}
