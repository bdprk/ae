//! Type rules shared by name resolution and compiler lowering.

use ae_syntax::Kind;
use crate::concrete::TyKind;
use crate::numeric::{FloatTy, IntTy};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Char,
}

/// Fixed-size arrays accept only i64 indices. An absent scalar here
/// denotes a known non-scalar type.
pub fn valid_array_index(index: Option<Scalar>) -> bool {
    index == Some(Scalar::Int(IntTy::I64))
}

/// Compound arithmetic stores its result back into the target's type.
pub fn scalar_compound_assignment(target: Option<Scalar>, operator: &Kind) -> bool {
    target.is_some_and(|ty| infix(operator, Some(ty), Some(ty)) == Ok(ty))
}

/// Explicit `as` scalar casts convert between numeric scalars (integers and floats).
pub fn valid_scalar_cast(from: Scalar, to: Scalar) -> bool {
    matches!(
        (from, to),
        (Scalar::Int(_), Scalar::Int(_))
            | (Scalar::Int(_), Scalar::Float(_))
            | (Scalar::Float(_), Scalar::Int(_))
            | (Scalar::Float(_), Scalar::Float(_))
            | (Scalar::Char, Scalar::Int(IntTy::U32))
            | (Scalar::Int(IntTy::U32), Scalar::Char)
            | (Scalar::Char, Scalar::Char)
    )
}

impl Scalar {
    pub fn from_spelling(name: &str) -> Option<Self> {
        match name {
            "i8" => Some(Self::Int(IntTy::I8)),
            "i16" => Some(Self::Int(IntTy::I16)),
            "i32" => Some(Self::Int(IntTy::I32)),
            "i64" => Some(Self::Int(IntTy::I64)),
            "isize" => Some(Self::Int(IntTy::Isize)),
            "u8" => Some(Self::Int(IntTy::U8)),
            "u16" => Some(Self::Int(IntTy::U16)),
            "u32" => Some(Self::Int(IntTy::U32)),
            "u64" => Some(Self::Int(IntTy::U64)),
            "usize" => Some(Self::Int(IntTy::Usize)),
            "f32" => Some(Self::Float(FloatTy::F32)),
            "f64" => Some(Self::Float(FloatTy::F64)),
            "bool" => Some(Self::Bool),
            "char" => Some(Self::Char),
            _ => None,
        }
    }

    /// The concrete type a checked scalar names.
    pub fn ty(self) -> TyKind {
        match self {
            Self::Int(width) => TyKind::Int(width),
            Self::Float(width) => TyKind::Float(width),
            Self::Bool => TyKind::Bool,
            Self::Char => TyKind::Char,
        }
    }

    pub fn spelling(self) -> &'static str {
        match self {
            Self::Int(width) => width.name(),
            Self::Float(width) => width.name(),
            Self::Bool => "bool",
            Self::Char => "char",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorError {
    Negation,
    UnsignedNegation,
    LogicalNot,
    LogicalAnd,
    LogicalOr,
    Bitwise,
    Comparison,
    MixedNumeric,
    MismatchedWidth,
    FloatRemainder,
    Arithmetic,
}

impl OperatorError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Negation => "unary '-' requires an i64 or f64 operand",
            Self::UnsignedNegation => "unary '-' is not allowed on unsigned integer operands",
            Self::LogicalNot => "'!' requires a boolean operand",
            Self::LogicalAnd => "'&&' requires boolean operands",
            Self::LogicalOr => "'||' requires boolean operands",
            Self::Bitwise => "bitwise operator requires i64 operands",
            Self::Comparison => "comparison requires two i64, two f64, or two strings/views with == or !=",
            Self::MixedNumeric => "mixed i64/f64 arithmetic is not allowed; convert one side explicitly with to_f64 or to_i64",
            Self::MismatchedWidth => "mixed integer widths are not allowed; convert one side explicitly with 'as'",
            Self::FloatRemainder => "'%' is defined for i64 operands only",
            Self::Arithmetic => "arithmetic requires integer operands",
        }
    }

    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::Comparison => Some("E0202"),
            Self::Negation | Self::UnsignedNegation | Self::Arithmetic | Self::MismatchedWidth => Some("E0203"),
            _ => None,
        }
    }
}

/// The caller supplies a parsed prefix operator. An absent scalar denotes
/// a known non-scalar type. An unresolved operand must defer before calling.
pub fn prefix(operator: &Kind, operand: Option<Scalar>) -> Result<Scalar, OperatorError> {
    use Scalar::*;
    match (operator, operand) {
        (Kind::Minus, Some(Int(width))) => {
            if width.signed() {
                Ok(Int(width))
            } else {
                Err(OperatorError::UnsignedNegation)
            }
        }
        (Kind::Minus, Some(ty @ Float(_))) => Ok(ty),
        (Kind::Minus, _) => Err(OperatorError::Negation),
        (Kind::Bang, Some(Bool)) => Ok(Bool),
        (Kind::Bang, _) => Err(OperatorError::LogicalNot),
        _ => unreachable!("parsed prefix operator"),
    }
}

/// The caller handles string/view operations with their ownership effects
/// before checking the remaining parsed infix operators here.
pub fn infix(operator: &Kind, left: Option<Scalar>, right: Option<Scalar>) -> Result<Scalar, OperatorError> {
    use Scalar::*;
    match operator {
        Kind::And | Kind::Or => {
            (left == Some(Bool) && right == Some(Bool)).then_some(Bool).ok_or(
                if *operator == Kind::And { OperatorError::LogicalAnd } else { OperatorError::LogicalOr },
            )
        }
        Kind::Pipe | Kind::Caret | Kind::Amp | Kind::Shl | Kind::Shr => match (left, right) {
            (Some(Int(l)), Some(Int(r))) if l == r => Ok(Int(l)),
            (Some(Int(_)), Some(Int(_))) => Err(OperatorError::MismatchedWidth),
            _ => Err(OperatorError::Bitwise),
        },
        Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq => {
            let comparable = matches!(
                (left, right),
                (Some(Int(l)), Some(Int(r))) if l == r
            ) || matches!(
                (left, right),
                (Some(Float(l)), Some(Float(r))) if l == r
            ) || (left == Some(Char) && right == Some(Char))
              || (left == Some(Bool) && right == Some(Bool) && matches!(operator, Kind::EqEq | Kind::NotEq));
            comparable.then_some(Bool).ok_or(OperatorError::Comparison)
        }
        Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent => match (left, right) {
            (Some(Int(l)), Some(Int(r))) if l == r => Ok(Int(l)),
            (Some(Int(_)), Some(Int(_))) => Err(OperatorError::MismatchedWidth),
            (Some(Float(l)), Some(Float(r))) if l == r => {
                if *operator == Kind::Percent {
                    Err(OperatorError::FloatRemainder)
                } else {
                    Ok(Float(l))
                }
            }
            (Some(Float(_)), Some(Float(_))) => Err(OperatorError::MismatchedWidth),
            (Some(Float(_)), _) | (_, Some(Float(_))) => Err(OperatorError::MixedNumeric),
            _ => Err(OperatorError::Arithmetic),
        },
        _ => unreachable!("parsed infix operator"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i8_u8_operator_rules() {
        let i8_t = Some(Scalar::Int(IntTy::I8));
        let u8_t = Some(Scalar::Int(IntTy::U8));
        let i64_t = Some(Scalar::Int(IntTy::I64));

        assert_eq!(prefix(&Kind::Minus, i8_t), Ok(Scalar::Int(IntTy::I8)));
        assert_eq!(prefix(&Kind::Minus, u8_t), Err(OperatorError::UnsignedNegation));

        assert_eq!(infix(&Kind::Plus, i8_t, i8_t), Ok(Scalar::Int(IntTy::I8)));
        assert_eq!(infix(&Kind::Plus, u8_t, u8_t), Ok(Scalar::Int(IntTy::U8)));
        assert_eq!(infix(&Kind::Plus, i8_t, i64_t), Err(OperatorError::MismatchedWidth));
        assert_eq!(infix(&Kind::Plus, i8_t, u8_t), Err(OperatorError::MismatchedWidth));

        assert_eq!(infix(&Kind::Shl, u8_t, u8_t), Ok(Scalar::Int(IntTy::U8)));
        assert_eq!(infix(&Kind::Amp, i8_t, u8_t), Err(OperatorError::MismatchedWidth));

        assert_eq!(infix(&Kind::Less, i8_t, i8_t), Ok(Scalar::Bool));
        assert_eq!(infix(&Kind::EqEq, u8_t, u8_t), Ok(Scalar::Bool));
        assert_eq!(infix(&Kind::EqEq, i8_t, u8_t), Err(OperatorError::Comparison));
        assert_eq!(infix(&Kind::Less, i8_t, i64_t), Err(OperatorError::Comparison));

        assert!(!valid_array_index(i8_t));
        assert!(!valid_array_index(u8_t));
        assert!(valid_array_index(i64_t));

        assert!(valid_scalar_cast(Scalar::Int(IntTy::I64), Scalar::Int(IntTy::I8)));
        assert!(valid_scalar_cast(Scalar::Int(IntTy::U8), Scalar::Float(FloatTy::F64)));
        assert!(!valid_scalar_cast(Scalar::Bool, Scalar::Int(IntTy::I8)));

        let i16_t = Some(Scalar::Int(IntTy::I16));
        let u16_t = Some(Scalar::Int(IntTy::U16));
        let i32_t = Some(Scalar::Int(IntTy::I32));
        let u32_t = Some(Scalar::Int(IntTy::U32));
        let isize_t = Some(Scalar::Int(IntTy::Isize));
        let u64_t = Some(Scalar::Int(IntTy::U64));
        let usize_t = Some(Scalar::Int(IntTy::Usize));

        assert_eq!(prefix(&Kind::Minus, i16_t), Ok(Scalar::Int(IntTy::I16)));
        assert_eq!(prefix(&Kind::Minus, i32_t), Ok(Scalar::Int(IntTy::I32)));
        assert_eq!(prefix(&Kind::Minus, isize_t), Ok(Scalar::Int(IntTy::Isize)));
        assert_eq!(prefix(&Kind::Minus, u16_t), Err(OperatorError::UnsignedNegation));
        assert_eq!(prefix(&Kind::Minus, u32_t), Err(OperatorError::UnsignedNegation));
        assert_eq!(prefix(&Kind::Minus, u64_t), Err(OperatorError::UnsignedNegation));
        assert_eq!(prefix(&Kind::Minus, usize_t), Err(OperatorError::UnsignedNegation));

        assert_eq!(infix(&Kind::Plus, isize_t, i64_t), Err(OperatorError::MismatchedWidth));
        assert_eq!(infix(&Kind::Plus, usize_t, u64_t), Err(OperatorError::MismatchedWidth));
        assert_eq!(infix(&Kind::Less, i32_t, u32_t), Err(OperatorError::Comparison));
        assert_eq!(infix(&Kind::Shr, u64_t, u64_t), Ok(Scalar::Int(IntTy::U64)));
    }
}
