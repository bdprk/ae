//! Structural type unification over the shared vocabulary (P1.2).
//!
//! Terms are concrete `TyKind`s, type variables, and the composite/pointer/
//! borrow constructors the checker reasons about. Unification produces a
//! substitution with an occurs check; it does no coercion (that is
//! [`crate::accepts_value`]'s separate question) and no instantiation (a
//! later pass turns the substitution into concrete ids). The module is the
//! single unifier the resolver, HIR and compiler will share, so a generic
//! call cannot be checked two different ways.

use crate::concrete::TyKind;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeVar(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Concrete(TyKind),
    Variable(TypeVar),
    /// A source-shaped composite: `vec[T]`, `map[K, V]`, `shared[T]`,
    /// `vec[string]` and the other named constructors share this carrier.
    Composite { family: &'static str, arguments: Vec<Term> },
    Pointer { mutable: bool, pointee: Box<Term> },
    Borrow { mutable: bool, inner: Box<Term> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifyError {
    /// Two rigid terms disagree.
    Mismatch,
    /// A variable would contain itself (`T` against `vec[T]`).
    Occurs(TypeVar),
}

#[derive(Debug, Default, Clone)]
pub struct Substitution {
    bindings: HashMap<TypeVar, Term>,
}

impl Substitution {
    pub fn new() -> Self {
        Self::default()
    }

    /// The term a variable currently stands for, following chains.
    pub fn binding(&self, var: TypeVar) -> Option<&Term> {
        self.bindings.get(&var)
    }

    /// Follow the substitution through a term without allocating for the
    /// common unbound case.
    pub fn resolved(&self, term: &Term) -> Term {
        match term {
            Term::Variable(var) => match self.bindings.get(var) {
                Some(bound) => self.resolved(bound),
                None => term.clone(),
            },
            Term::Composite { family, arguments } => Term::Composite {
                family,
                arguments: arguments.iter().map(|argument| self.resolved(argument)).collect(),
            },
            Term::Pointer { mutable, pointee } => Term::Pointer {
                mutable: *mutable,
                pointee: Box::new(self.resolved(pointee)),
            },
            Term::Borrow { mutable, inner } => Term::Borrow {
                mutable: *mutable,
                inner: Box::new(self.resolved(inner)),
            },
            Term::Concrete(_) => term.clone(),
        }
    }
}

/// Unify two terms, extending `substitution` in place. On failure the
/// substitution may hold bindings made earlier in the same call; callers
/// that need atomicity work on a clone.
pub fn unify(left: &Term, right: &Term, substitution: &mut Substitution) -> Result<(), UnifyError> {
    let left = substitution.resolved(left);
    let right = substitution.resolved(right);
    match (&left, &right) {
        (Term::Variable(a), Term::Variable(b)) if a == b => Ok(()),
        (Term::Variable(var), term) | (term, Term::Variable(var)) => bind(*var, term, substitution),
        (Term::Concrete(a), Term::Concrete(b)) if a == b => Ok(()),
        (
            Term::Composite { family: a_family, arguments: a_arguments },
            Term::Composite { family: b_family, arguments: b_arguments },
        ) if a_family == b_family && a_arguments.len() == b_arguments.len() => {
            for (a, b) in a_arguments.iter().zip(b_arguments) {
                unify(a, b, substitution)?;
            }
            Ok(())
        }
        (
            Term::Pointer { mutable: a_mutable, pointee: a_pointee },
            Term::Pointer { mutable: b_mutable, pointee: b_pointee },
        ) if a_mutable == b_mutable => unify(a_pointee, b_pointee, substitution),
        (
            Term::Borrow { mutable: a_mutable, inner: a_inner },
            Term::Borrow { mutable: b_mutable, inner: b_inner },
        ) if a_mutable == b_mutable => unify(a_inner, b_inner, substitution),
        _ => Err(UnifyError::Mismatch),
    }
}

fn bind(var: TypeVar, term: &Term, substitution: &mut Substitution) -> Result<(), UnifyError> {
    if occurs(var, term) {
        return Err(UnifyError::Occurs(var));
    }
    substitution.bindings.insert(var, term.clone());
    Ok(())
}

fn occurs(var: TypeVar, term: &Term) -> bool {
    match term {
        Term::Variable(other) => *other == var,
        Term::Concrete(_) => false,
        Term::Composite { arguments, .. } => arguments.iter().any(|argument| occurs(var, argument)),
        Term::Pointer { pointee, .. } => occurs(var, pointee),
        Term::Borrow { inner, .. } => occurs(var, inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numeric::{FloatTy, IntTy};

    fn var(id: u32) -> Term {
        Term::Variable(TypeVar(id))
    }

    #[test]
    fn concrete_terms_unify_only_when_identical() {
        let mut subst = Substitution::new();
        assert_eq!(unify(&Term::Concrete(TyKind::Int(IntTy::I64)), &Term::Concrete(TyKind::Int(IntTy::I64)), &mut subst), Ok(()));
        assert_eq!(unify(&Term::Concrete(TyKind::Int(IntTy::I64)), &Term::Concrete(TyKind::Float(FloatTy::F64)), &mut subst), Err(UnifyError::Mismatch));
    }

    #[test]
    fn a_variable_takes_the_concrete_shape_and_composites_descend() {
        let vector_i64 = Term::Composite { family: "vec", arguments: vec![Term::Concrete(TyKind::Int(IntTy::I64))] };
        let vector_t = Term::Composite { family: "vec", arguments: vec![var(0)] };
        let mut subst = Substitution::new();
        assert_eq!(unify(&vector_t, &vector_i64, &mut subst), Ok(()));
        assert_eq!(subst.resolved(&var(0)), Term::Concrete(TyKind::Int(IntTy::I64)));
        assert_eq!(subst.resolved(&vector_t), vector_i64);
    }

    #[test]
    fn family_and_flavor_must_agree() {
        let mut subst = Substitution::new();
        let vector = Term::Composite { family: "vec", arguments: vec![var(0)] };
        let map = Term::Composite { family: "map", arguments: vec![var(1), var(0)] };
        assert_eq!(unify(&vector, &map, &mut subst), Err(UnifyError::Mismatch));
        let pointer = Term::Pointer { mutable: true, pointee: Box::new(var(0)) };
        let shared = Term::Pointer { mutable: false, pointee: Box::new(var(0)) };
        assert_eq!(unify(&pointer, &shared, &mut Substitution::new()), Err(UnifyError::Mismatch));
        let borrow = Term::Borrow { mutable: true, inner: Box::new(var(0)) };
        assert_eq!(unify(&pointer, &borrow, &mut Substitution::new()), Err(UnifyError::Mismatch));
    }

    #[test]
    fn chains_resolve_and_self_reference_is_an_occurs_error() {
        let mut subst = Substitution::new();
        assert_eq!(unify(&var(0), &var(1), &mut subst), Ok(()));
        assert_eq!(unify(&var(1), &Term::Concrete(TyKind::Bool), &mut subst), Ok(()));
        assert_eq!(subst.resolved(&var(0)), Term::Concrete(TyKind::Bool));

        let recursive = Term::Composite { family: "vec", arguments: vec![var(2)] };
        assert_eq!(unify(&var(2), &recursive, &mut subst), Err(UnifyError::Occurs(TypeVar(2))));
    }
}