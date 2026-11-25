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
    vector_elems: Vec<TyKind>,
    map_types: Vec<(TyKind, TyKind)>,
    array_elems: Vec<TyKind>,
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

    /// ADR-0235: the element type of a generic vector (`Vector(ElemId)`).
    pub fn intern_vector_elem(&mut self, element: TyKind) -> usize {
        intern(&mut self.vector_elems, element)
    }

    pub fn vector_elem(&self, id: usize) -> TyKind {
        self.vector_elems[id]
    }

    pub fn vector_elem_count(&self) -> usize {
        self.vector_elems.len()
    }

    /// ADR-0235: the key and value types of a generic map (`MapOf(MapId)`).
    pub fn intern_map_types(&mut self, key: TyKind, value: TyKind) -> usize {
        if let Some(index) = self.map_types.iter().position(|&(k, v)| k == key && v == value) {
            return index;
        }
        self.map_types.push((key, value));
        self.map_types.len() - 1
    }

    pub fn map_types(&self, id: usize) -> (TyKind, TyKind) {
        self.map_types[id]
    }

    pub fn find_map_types(&self, key: TyKind, value: TyKind) -> Option<usize> {
        self.map_types.iter().position(|&(k, v)| k == key && v == value)
    }

    pub fn map_types_count(&self) -> usize {
        self.map_types.len()
    }

    /// ADR-0235: the element type of a fixed-size array (`ArrayOf(ElemId, len)`).
    pub fn intern_array_elem(&mut self, element: TyKind) -> usize {
        intern(&mut self.array_elems, element)
    }

    pub fn array_elem(&self, id: usize) -> TyKind {
        self.array_elems[id]
    }

    pub fn find_array_elem(&self, element: TyKind) -> Option<usize> {
        self.array_elems.iter().position(|existing| *existing == element)
    }

    pub fn array_elem_count(&self) -> usize {
        self.array_elems.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_element_ids_are_structural_and_ordered() {
        let mut interner = TypeInterner::new();
        let int = TyKind::Int(crate::numeric::IntTy::I64);
        let first = interner.intern_vector_elem(int);
        let again = interner.intern_vector_elem(int);
        let text = interner.intern_vector_elem(TyKind::String);
        assert_eq!(first, again, "the same element type reuses its id");
        assert_ne!(first, text, "different element types stay distinct");
        assert_eq!(first, 0);
        assert_eq!(text, 1);
        assert_eq!(interner.vector_elem(first), int);
        assert_eq!(interner.vector_elem_count(), 2);
    }

    #[test]
    fn map_and_array_ids_are_structural_and_ordered() {
        let mut interner = TypeInterner::new();
        let int = TyKind::Int(crate::numeric::IntTy::I64);
        let float = TyKind::Float(crate::numeric::FloatTy::F64);
        let m_int = interner.intern_map_types(TyKind::String, int);
        let m_again = interner.intern_map_types(TyKind::String, int);
        let m_float = interner.intern_map_types(TyKind::String, float);
        assert_eq!(m_int, m_again);
        assert_ne!(m_int, m_float);
        assert_eq!(m_int, 0);
        assert_eq!(m_float, 1);
        assert_eq!(interner.map_types(m_int), (TyKind::String, int));
        assert_eq!(interner.find_map_types(TyKind::String, float), Some(m_float));
        assert_eq!(interner.find_map_types(int, int), None);
        assert_eq!(interner.map_types_count(), 2);

        let a_int = interner.intern_array_elem(int);
        let a_again = interner.intern_array_elem(int);
        let a_float = interner.intern_array_elem(float);
        assert_eq!(a_int, a_again);
        assert_ne!(a_int, a_float);
        assert_eq!(interner.array_elem(a_int), int);
        assert_eq!(interner.find_array_elem(float), Some(a_float));
        assert_eq!(interner.find_array_elem(TyKind::Bool), None);
        assert_eq!(interner.array_elem_count(), 2);
    }
}
