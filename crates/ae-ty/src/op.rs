//! Type rules shared by name resolution and compiler lowering.

use ae_syntax::Kind;
use crate::concrete::TyKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Int,
    Float,
    Bool,
}

/// Fixed-size arrays accept only i64 indices. An absent scalar here
/// denotes a known non-scalar type.
pub fn valid_array_index(index: Option<Scalar>) -> bool {
    index == Some(Scalar::Int)
}

/// Compound arithmetic stores its result back into the target's type.
pub fn scalar_compound_assignment(target: Option<Scalar>, operator: &Kind) -> bool {
    target.is_some_and(|ty| infix(operator, Some(ty), Some(ty)) == Ok(ty))
}

impl Scalar {
    pub fn from_spelling(name: &str) -> Option<Self> {
        match name {
            "i64" => Some(Self::Int),
            "f64" => Some(Self::Float),
            "bool" => Some(Self::Bool),
            _ => None,
        }
    }

    /// The concrete type a checked scalar names.
    pub fn ty(self) -> TyKind {
        match self {
            Self::Int => TyKind::Int(crate::numeric::IntTy::I64),
            Self::Float => TyKind::Float(crate::numeric::FloatTy::F64),
            Self::Bool => TyKind::Bool,
        }
    }

    pub fn spelling(self) -> &'static str {
        match self {
            Self::Int => "i64",
            Self::Float => "f64",
            Self::Bool => "bool",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorError {
    Negation,
    LogicalNot,
    LogicalAnd,
    LogicalOr,
    Bitwise,
    Comparison,
    MixedNumeric,
    FloatRemainder,
    Arithmetic,
}

impl OperatorError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Negation => "unary '-' requires an i64 or f64 operand",
            Self::LogicalNot => "'!' requires a boolean operand",
            Self::LogicalAnd => "'&&' requires boolean operands",
            Self::LogicalOr => "'||' requires boolean operands",
            Self::Bitwise => "bitwise operator requires i64 operands",
            Self::Comparison => "comparison requires two i64, two f64, or two strings/views with == or !=",
            Self::MixedNumeric => "mixed i64/f64 arithmetic is not allowed; convert one side explicitly with to_f64 or to_i64",
            Self::FloatRemainder => "'%' is defined for i64 operands only",
            Self::Arithmetic => "arithmetic requires integer operands",
        }
    }

    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::Comparison => Some("E0202"),
            Self::Negation | Self::Arithmetic => Some("E0203"),
            _ => None,
        }
    }
}

/// The caller supplies a parsed prefix operator. An absent scalar denotes
/// a known non-scalar type. An unresolved operand must defer before calling.
pub fn prefix(operator: &Kind, operand: Option<Scalar>) -> Result<Scalar, OperatorError> {
    use Scalar::*;
    match (operator, operand) {
        (Kind::Minus, Some(ty @ (Int | Float))) => Ok(ty),
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
        Kind::Pipe | Kind::Caret | Kind::Amp | Kind::Shl | Kind::Shr => {
            (left == Some(Int) && right == Some(Int)).then_some(Int).ok_or(OperatorError::Bitwise)
        }
        Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq => {
            let comparable = matches!((left, right), (Some(Int), Some(Int)) | (Some(Float), Some(Float)))
                || (left == Some(Bool) && right == Some(Bool) && matches!(operator, Kind::EqEq | Kind::NotEq));
            comparable.then_some(Bool).ok_or(OperatorError::Comparison)
        }
        Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent => match (left, right) {
            (Some(Int), Some(Int)) => Ok(Int),
            (Some(Float), Some(Float)) if *operator == Kind::Percent => Err(OperatorError::FloatRemainder),
            (Some(Float), Some(Float)) => Ok(Float),
            (Some(Float), _) | (_, Some(Float)) => Err(OperatorError::MixedNumeric),
            _ => Err(OperatorError::Arithmetic),
        },
        _ => unreachable!("parsed infix operator"),
    }
}
