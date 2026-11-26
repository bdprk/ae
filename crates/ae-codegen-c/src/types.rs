//! Type discovery, C11 type mapping, and topological `typedef` emission.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ae_mir::{
    AggregateKind, Body, Operand, Place, ProjectionElem, Rvalue, StatementKind,
};
use ae_ty::{FloatTy, IntTy, TyKind};

use crate::CodegenError;

/// Returns `(min_expr, max_expr)` C11 constant expressions for `ity`.
pub(crate) fn int_c_bounds(ity: IntTy) -> (&'static str, &'static str) {
    match ity {
        IntTy::I8 => ("INT8_MIN", "INT8_MAX"),
        IntTy::I16 => ("INT16_MIN", "INT16_MAX"),
        IntTy::I32 => ("INT32_MIN", "INT32_MAX"),
        IntTy::I64 => ("INT64_MIN", "INT64_MAX"),
        IntTy::Isize => ("INTPTR_MIN", "INTPTR_MAX"),
        IntTy::U8 => ("0", "UINT8_MAX"),
        IntTy::U16 => ("0", "UINT16_MAX"),
        IntTy::U32 => ("0", "UINT32_MAX"),
        IntTy::U64 => ("0", "UINT64_MAX"),
        IntTy::Usize => ("0", "UINTPTR_MAX"),
    }
}

/// Maps a [`TyKind`] to its C11 type name.
pub(crate) fn c_type_name(ty: TyKind) -> String {
    match ty {
        TyKind::Int(w) => w.c_name().to_owned(),
        TyKind::Float(FloatTy::F32) => "float".to_owned(),
        TyKind::Float(FloatTy::F64) => "double".to_owned(),
        TyKind::Bool => "bool".to_owned(),
        TyKind::Char => "uint32_t".to_owned(),
        TyKind::Unit => "ae_unit_t".to_owned(),
        TyKind::String | TyKind::StrView => "const char *".to_owned(),
        TyKind::Struct(id) => format!("ae_struct_{id}"),
        TyKind::Closure(id) => format!("ae_closure_{id}"),
        TyKind::Enum(_, id) => format!("ae_enum_{id}"),
        TyKind::ArrayOf(elem_id, len) => format!("ae_array_{elem_id}_{len}"),
        TyKind::Vector(_)
        | TyKind::VectorRef(_)
        | TyKind::MapOf(_)
        | TyKind::Slice(_)
        | TyKind::SliceMut(_)
        | TyKind::StructRef(_)
        | TyKind::StructMut(_)
        | TyKind::Tokens
        | TyKind::Fn(_)
        | TyKind::Ptr(_)
        | TyKind::PtrMut(_)
        | TyKind::Thread(_)
        | TyKind::Atomic
        | TyKind::Shared(_)
        | TyKind::Mutex
        | TyKind::Guard
        | TyKind::Chan
        | TyKind::Exec
        | TyKind::TaskScope
        | TyKind::Pool => "void *".to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TypeDefKey {
    Struct(usize),
    Closure(usize),
    Enum(usize),
    Array(usize, usize),
}

/// Discovered aggregate/array layouts across all bodies in a module.
#[derive(Debug, Default)]
pub(crate) struct ModuleTypes {
    pub structs: BTreeMap<usize, Vec<Option<TyKind>>>,
    pub closures: BTreeMap<usize, Vec<Option<TyKind>>>,
    pub enums: BTreeMap<usize, BTreeMap<usize, Vec<Option<TyKind>>>>,
    pub arrays: BTreeMap<(usize, usize), Option<TyKind>>,
}

impl ModuleTypes {
    pub fn collect(bodies: &[Body]) -> Self {
        let mut types = Self::default();

        // Register all declared nominal/array types first.
        for body in bodies {
            for decl in &body.locals {
                types.register_ty(decl.ty);
            }
        }

        // Fixed-point pass (up to 4 iterations) to propagate field/element types
        // from Aggregates and Place projections.
        for _ in 0..4 {
            for body in bodies {
                types.scan_body(body);
            }
        }

        types
    }

    fn register_ty(&mut self, ty: TyKind) {
        match ty {
            TyKind::Struct(id) => {
                self.structs.entry(id).or_default();
            }
            TyKind::Closure(id) => {
                self.closures.entry(id).or_default();
            }
            TyKind::Enum(_, id) => {
                self.enums.entry(id).or_default();
            }
            TyKind::ArrayOf(elem_id, len) => {
                self.arrays.entry((elem_id, len)).or_insert(None);
            }
            _ => {}
        }
    }

    fn scan_body(&mut self, body: &Body) {
        for bb in &body.basic_blocks {
            for stmt in &bb.statements {
                match &stmt.kind {
                    StatementKind::Assign(place, rvalue) => {
                        self.scan_rvalue(body, rvalue);
                        if let Some(rhs_ty) = self.infer_rvalue_ty(body, rvalue) {
                            self.record_place_write_ty(body, place, rhs_ty);
                        }
                    }
                    StatementKind::SetDiscriminant {
                        place,
                        variant_index,
                    } => {
                        if let Some(TyKind::Enum(_, id)) = self.infer_place_ty(body, place) {
                            self.enums
                                .entry(id)
                                .or_default()
                                .entry(*variant_index)
                                .or_default();
                        }
                    }
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Drop(_)
                    | StatementKind::Nop => {}
                }
            }
        }
    }

    fn scan_rvalue(&mut self, body: &Body, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Aggregate(kind, ops) => {
                let op_tys: Vec<Option<TyKind>> =
                    ops.iter().map(|op| self.infer_operand_ty(body, op)).collect();
                match kind {
                    AggregateKind::Struct(id) => {
                        let fields = self.structs.entry(*id).or_default();
                        ensure_field_slots(fields, &op_tys);
                    }
                    AggregateKind::Closure(id) => {
                        let fields = self.closures.entry(*id).or_default();
                        ensure_field_slots(fields, &op_tys);
                    }
                    AggregateKind::Enum(id, variant) => {
                        let variants = self.enums.entry(*id).or_default();
                        let fields = variants.entry(*variant).or_default();
                        ensure_field_slots(fields, &op_tys);
                    }
                    AggregateKind::Array(elem_ty) => {
                        self.register_ty(*elem_ty);
                        let len = ops.len();
                        for ((_, klen), slot) in &mut self.arrays {
                            if *klen == len && slot.is_none() {
                                *slot = Some(*elem_ty);
                            }
                        }
                    }
                    AggregateKind::Tuple => {}
                }
            }
            Rvalue::Cast(_, _, target_ty) => {
                self.register_ty(*target_ty);
            }
            _ => {}
        }
    }

    fn record_place_write_ty(&mut self, body: &Body, place: &Place, written_ty: TyKind) {
        let Some(base_decl) = body.local_decl(place.local) else {
            return;
        };
        let mut cur_ty = base_decl.ty;
        let mut active_variant: Option<usize> = None;

        for (i, elem) in place.projection.iter().enumerate() {
            let is_last = i + 1 == place.projection.len();
            match elem {
                ProjectionElem::Downcast(v) => {
                    active_variant = Some(*v);
                    if let TyKind::Enum(_, id) = cur_ty {
                        self.enums.entry(id).or_default().entry(*v).or_default();
                    }
                }
                ProjectionElem::Field(fidx) => match cur_ty {
                    TyKind::Struct(id) => {
                        let fields = self.structs.entry(id).or_default();
                        if fields.len() <= *fidx {
                            fields.resize(*fidx + 1, None);
                        }
                        if is_last && fields[*fidx].is_none() {
                            fields[*fidx] = Some(written_ty);
                        }
                        if let Some(next_ty) = fields.get(*fidx).and_then(|t| *t) {
                            cur_ty = next_ty;
                        } else {
                            return;
                        }
                    }
                    TyKind::Closure(id) => {
                        let fields = self.closures.entry(id).or_default();
                        if fields.len() <= *fidx {
                            fields.resize(*fidx + 1, None);
                        }
                        if is_last && fields[*fidx].is_none() {
                            fields[*fidx] = Some(written_ty);
                        }
                        if let Some(next_ty) = fields.get(*fidx).and_then(|t| *t) {
                            cur_ty = next_ty;
                        } else {
                            return;
                        }
                    }
                    TyKind::Enum(_, id) => {
                        let v = active_variant.unwrap_or(0);
                        let fields = self.enums.entry(id).or_default().entry(v).or_default();
                        if fields.len() <= *fidx {
                            fields.resize(*fidx + 1, None);
                        }
                        if is_last && fields[*fidx].is_none() {
                            fields[*fidx] = Some(written_ty);
                        }
                        if let Some(next_ty) = fields.get(*fidx).and_then(|t| *t) {
                            cur_ty = next_ty;
                        } else {
                            return;
                        }
                    }
                    _ => return,
                },
                ProjectionElem::Index(_) => {
                    if let TyKind::ArrayOf(elem_id, len) = cur_ty {
                        if is_last {
                            self.arrays
                                .entry((elem_id, len))
                                .and_modify(|slot| {
                                    if slot.is_none() {
                                        *slot = Some(written_ty);
                                    }
                                })
                                .or_insert(Some(written_ty));
                        }
                        if let Some(elem_ty) = self.arrays.get(&(elem_id, len)).and_then(|t| *t) {
                            cur_ty = elem_ty;
                        } else {
                            return;
                        }
                    } else {
                        return;
                    }
                }
                ProjectionElem::Deref => return,
            }
        }

        // Also match ArrayOf(elem_id, len) when assigning an AggregateKind::Array directly
        if place.projection.is_empty()
            && let TyKind::ArrayOf(elem_id, len) = base_decl.ty
            && let Some(elem_ty) = self.arrays.get(&(elem_id, len)).and_then(|t| *t)
        {
            self.arrays.insert((elem_id, len), Some(elem_ty));
        }
    }

    pub fn infer_operand_ty(&self, body: &Body, op: &Operand) -> Option<TyKind> {
        match op {
            Operand::Copy(place) | Operand::Move(place) => self.infer_place_ty(body, place),
            Operand::Constant(c) => Some(c.ty),
        }
    }

    pub fn infer_rvalue_ty(&self, body: &Body, rvalue: &Rvalue) -> Option<TyKind> {
        match rvalue {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) => self.infer_operand_ty(body, op),
            Rvalue::BinaryOp(op, lhs, _) => {
                if op.is_comparison() {
                    Some(TyKind::Bool)
                } else {
                    self.infer_operand_ty(body, lhs)
                }
            }
            Rvalue::Cast(_, _, target_ty) => Some(*target_ty),
            Rvalue::Aggregate(kind, ops) => match kind {
                AggregateKind::Struct(id) => Some(TyKind::Struct(*id)),
                AggregateKind::Closure(id) => Some(TyKind::Closure(*id)),
                AggregateKind::Enum(id, _) => Some(TyKind::Enum("Enum", *id)),
                AggregateKind::Array(elem_ty) => {
                    // Try to find matching ArrayOf in arrays
                    for ((elem_id, len), slot) in &self.arrays {
                        if *len == ops.len() && *slot == Some(*elem_ty) {
                            return Some(TyKind::ArrayOf(*elem_id, *len));
                        }
                    }
                    None
                }
                AggregateKind::Tuple => None,
            },
            Rvalue::Len(_) => Some(TyKind::Int(IntTy::Usize)),
            Rvalue::Discriminant(_) => Some(TyKind::Int(IntTy::I64)),
            Rvalue::Ref { .. } => Some(TyKind::Ptr(0)),
        }
    }

    pub fn infer_place_ty(&self, body: &Body, place: &Place) -> Option<TyKind> {
        let mut cur_ty = body.local_decl(place.local)?.ty;
        let mut active_variant: Option<usize> = None;

        for elem in &place.projection {
            match elem {
                ProjectionElem::Downcast(v) => {
                    active_variant = Some(*v);
                }
                ProjectionElem::Field(fidx) => match cur_ty {
                    TyKind::Struct(id) => {
                        cur_ty = self.structs.get(&id)?.get(*fidx).and_then(|t| *t)?;
                    }
                    TyKind::Closure(id) => {
                        cur_ty = self.closures.get(&id)?.get(*fidx).and_then(|t| *t)?;
                    }
                    TyKind::Enum(_, id) => {
                        let v = active_variant.unwrap_or(0);
                        cur_ty = self
                            .enums
                            .get(&id)?
                            .get(&v)?
                            .get(*fidx)
                            .and_then(|t| *t)?;
                    }
                    _ => return None,
                },
                ProjectionElem::Index(_) => match cur_ty {
                    TyKind::ArrayOf(elem_id, len) => {
                        cur_ty = self.arrays.get(&(elem_id, len)).and_then(|t| *t)?;
                    }
                    _ => return None,
                },
                ProjectionElem::Deref => return None,
            }
        }
        Some(cur_ty)
    }

    pub fn emit_typedefs(&self, out: &mut String) -> Result<(), CodegenError> {
        let mut all_keys = BTreeSet::new();
        for &id in self.structs.keys() {
            all_keys.insert(TypeDefKey::Struct(id));
        }
        for &id in self.closures.keys() {
            all_keys.insert(TypeDefKey::Closure(id));
        }
        for &id in self.enums.keys() {
            all_keys.insert(TypeDefKey::Enum(id));
        }
        for &(elem_id, len) in self.arrays.keys() {
            all_keys.insert(TypeDefKey::Array(elem_id, len));
        }

        let mut visited = BTreeSet::new();
        let mut visiting = BTreeSet::new();

        for key in all_keys {
            self.emit_typedef_topo(key, out, &mut visited, &mut visiting)?;
        }
        if !visited.is_empty() {
            out.push('\n');
        }
        Ok(())
    }

    fn emit_typedef_topo(
        &self,
        key: TypeDefKey,
        out: &mut String,
        visited: &mut BTreeSet<TypeDefKey>,
        visiting: &mut BTreeSet<TypeDefKey>,
    ) -> Result<(), CodegenError> {
        if visited.contains(&key) {
            return Ok(());
        }
        if !visiting.insert(key) {
            return Err(CodegenError::TypeCycle {
                detail: format!("cyclic value type dependency involving {key:?}"),
            });
        }

        for dep in self.direct_deps(key) {
            self.emit_typedef_topo(dep, out, visited, visiting)?;
        }

        visiting.remove(&key);
        visited.insert(key);
        self.emit_single_typedef(key, out);
        Ok(())
    }

    fn direct_deps(&self, key: TypeDefKey) -> Vec<TypeDefKey> {
        let mut deps = Vec::new();
        let mut push_ty = |ty: TyKind| match ty {
            TyKind::Struct(id) => deps.push(TypeDefKey::Struct(id)),
            TyKind::Closure(id) => deps.push(TypeDefKey::Closure(id)),
            TyKind::Enum(_, id) => deps.push(TypeDefKey::Enum(id)),
            TyKind::ArrayOf(elem_id, len) => deps.push(TypeDefKey::Array(elem_id, len)),
            _ => {}
        };

        match key {
            TypeDefKey::Struct(id) => {
                if let Some(fields) = self.structs.get(&id) {
                    for f in fields.iter().flatten() {
                        push_ty(*f);
                    }
                }
            }
            TypeDefKey::Closure(id) => {
                if let Some(fields) = self.closures.get(&id) {
                    for f in fields.iter().flatten() {
                        push_ty(*f);
                    }
                }
            }
            TypeDefKey::Enum(id) => {
                if let Some(variants) = self.enums.get(&id) {
                    for fields in variants.values() {
                        for f in fields.iter().flatten() {
                            push_ty(*f);
                        }
                    }
                }
            }
            TypeDefKey::Array(elem_id, len) => {
                if let Some(Some(elem_ty)) = self.arrays.get(&(elem_id, len)) {
                    push_ty(*elem_ty);
                }
            }
        }
        deps
    }

    fn emit_single_typedef(&self, key: TypeDefKey, out: &mut String) {
        match key {
            TypeDefKey::Struct(id) => {
                let fields = self.structs.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                emit_struct_typedef(out, &format!("ae_struct_{id}"), fields);
            }
            TypeDefKey::Closure(id) => {
                let fields = self.closures.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                emit_struct_typedef(out, &format!("ae_closure_{id}"), fields);
            }
            TypeDefKey::Enum(id) => {
                let variants = self.enums.get(&id);
                let _ = writeln!(out, "typedef struct {{");
                let _ = writeln!(out, "    int32_t tag;");
                let _ = writeln!(out, "    union {{");
                let mut has_fields = false;
                if let Some(variants) = variants {
                    for (v_idx, fields) in variants {
                        if !fields.is_empty() {
                            has_fields = true;
                            let _ = writeln!(out, "        struct {{");
                            for (f_idx, f_ty) in fields.iter().enumerate() {
                                let c_ty = c_type_name(f_ty.unwrap_or(TyKind::Int(IntTy::I64)));
                                let _ = writeln!(out, "            {c_ty} f{f_idx};");
                            }
                            let _ = writeln!(out, "        }} v{v_idx};");
                        }
                    }
                }
                if !has_fields {
                    let _ = writeln!(out, "        uint8_t _empty;");
                }
                let _ = writeln!(out, "    }} u;");
                let _ = writeln!(out, "}} ae_enum_{id};");
            }
            TypeDefKey::Array(elem_id, len) => {
                let elem_ty = self
                    .arrays
                    .get(&(elem_id, len))
                    .and_then(|t| *t)
                    .unwrap_or(TyKind::Int(IntTy::I64));
                let c_ty = c_type_name(elem_ty);
                if len == 0 {
                    let _ = writeln!(
                        out,
                        "typedef struct {{ uint8_t _empty; }} ae_array_{elem_id}_{len};"
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "typedef struct {{ {c_ty} data[{len}]; }} ae_array_{elem_id}_{len};"
                    );
                }
            }
        }
    }
}

fn emit_struct_typedef(out: &mut String, name: &str, fields: &[Option<TyKind>]) {
    if fields.is_empty() {
        let _ = writeln!(out, "typedef struct {{ uint8_t _empty; }} {name};");
        return;
    }
    let _ = writeln!(out, "typedef struct {{");
    for (idx, f_ty) in fields.iter().enumerate() {
        let c_ty = c_type_name(f_ty.unwrap_or(TyKind::Int(IntTy::I64)));
        let _ = writeln!(out, "    {c_ty} f{idx};");
    }
    let _ = writeln!(out, "}} {name};");
}

fn ensure_field_slots(fields: &mut Vec<Option<TyKind>>, op_tys: &[Option<TyKind>]) {
    if fields.len() < op_tys.len() {
        fields.resize(op_tys.len(), None);
    }
    for (slot, ty) in fields.iter_mut().zip(op_tys.iter()) {
        if slot.is_none() {
            *slot = *ty;
        }
    }
}
