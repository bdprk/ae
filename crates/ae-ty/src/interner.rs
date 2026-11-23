//! Per-compilation type arenas (P1.2). One `TypeInterner` belongs to one
//! compilation: pointer pointees, shared boxes and function signatures are
//! interned here by structural equality, in first-seen order, so an id is
//! stable for the lifetime of that compilation and never global.

use crate::concrete::TyKind;

#[derive(Debug, Default, Clone)]
pub struct TypeInterner {
    ptr_pointees: Vec<TyKind>,
    shared: Vec<TyKind>,
    fn_sigs: Vec<(Vec<TyKind>, TyKind)>,
}

impl TypeInterner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern_pointee(&mut self, pointee: TyKind) -> usize {
        intern(&mut self.ptr_pointees, pointee)
    }

    pub fn pointee_count(&self) -> usize {
        self.ptr_pointees.len()
    }

    pub fn find_pointee(&self, pointee: TyKind) -> Option<usize> {
        self.ptr_pointees.iter().position(|existing| *existing == pointee)
    }

    pub fn pointee(&self, id: usize) -> TyKind {
        self.ptr_pointees[id]
    }

    pub fn intern_shared(&mut self, element: TyKind) -> usize {
        intern(&mut self.shared, element)
    }

    pub fn shared(&self, id: usize) -> TyKind {
        self.shared[id]
    }

    pub fn intern_fn_sig(&mut self, params: Vec<TyKind>, ret: TyKind) -> usize {
        if let Some(index) = self.fn_sigs.iter().position(|(existing, existing_ret)| *existing == params && *existing_ret == ret) {
            return index;
        }
        self.fn_sigs.push((params, ret));
        self.fn_sigs.len() - 1
    }

    /// The signature an interned id names. A synthetic test expression or a
    /// walker meeting an id it did not create must not panic, so this
    /// answers only for ids this interner created.
    pub fn fn_sig_checked(&self, id: usize) -> Option<(Vec<TyKind>, TyKind)> {
        self.fn_sigs.get(id).cloned()
    }

    pub fn fn_sig(&self, id: usize) -> (Vec<TyKind>, TyKind) {
        self.fn_sigs[id].clone()
    }
}

fn intern(arena: &mut Vec<TyKind>, value: TyKind) -> usize {
    if let Some(index) = arena.iter().position(|existing| *existing == value) {
        return index;
    }
    arena.push(value);
    arena.len() - 1
}
