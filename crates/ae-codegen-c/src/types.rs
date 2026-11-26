//! Type discovery, C11 type mapping, and topological `typedef` emission.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ae_mir::{
    AggregateKind, Body, ConstValue, Operand, Place, ProjectionElem, Rvalue, StatementKind,
    TerminatorKind,
};
use ae_ty::{DefId, FloatTy, IntTy, TyKind};

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
        // Owned and borrowed strings share one C representation (ADR-0238):
        // the `owned` flag says whether this value must free the bytes.
        TyKind::String | TyKind::StrView => "ae_str".to_owned(),
        TyKind::Struct(id) => format!("ae_struct_{id}"),
        // A closure value is the lifted function's address: the legacy
        // backend spells it `void` and calls it through a resolved function
        // name, and the pure pipeline carries the same address constant.
        TyKind::Closure(_) => "void *".to_owned(),
        TyKind::Enum(_, id) => format!("ae_enum_{id}"),
        TyKind::ArrayOf(elem_id, len) => format!("ae_array_{elem_id}_{len}"),
        // A function value is a typed function pointer (typedef emitted per
        // signature), never an opaque `void *`.
        TyKind::Fn(sig) => format!("ae_fn_t{sig}"),
        TyKind::Vector(_) => "ae_vec".to_owned(),
        // A borrowed vector is a pointer to the owner's header; indexing and
        // length reads go through the pointer.
        TyKind::VectorRef(_) => "ae_vec *".to_owned(),
        // A slice is a fat pointer value; mutable windows share the header
        // and differ only in what the source allows.
        TyKind::Slice(_) | TyKind::SliceMut(_) => "ae_slice".to_owned(),
        TyKind::Tokens => "ae_tokens".to_owned(),
        TyKind::Atomic => "ae_atomic".to_owned(),
        TyKind::MapOf(_) => "ae_map".to_owned(),
        TyKind::Exec => "ae_exec".to_owned(),
        TyKind::Thread(_) => "ae_thread_slot *".to_owned(),
        TyKind::Shared(_) => "ae_shared".to_owned(),
        TyKind::Mutex => "ae_mutex".to_owned(),
        TyKind::Guard => "ae_guard".to_owned(),
        TyKind::Chan => "ae_chan".to_owned(),
        TyKind::TaskScope => "ae_task_scope".to_owned(),
        TyKind::Pool => "ae_pool".to_owned(),
        TyKind::StructRef(_)
        | TyKind::StructMut(_)
        | TyKind::Ptr(_)
        | TyKind::PtrMut(_) => "void *".to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TypeDefKey {
    Struct(usize),
    Closure(usize),
    Enum(usize),
    Array(usize, usize),
    FnSig(usize),
}

/// Discovered aggregate/array layouts across all bodies in a module.
#[derive(Debug, Default)]
pub(crate) struct ModuleTypes {
    pub structs: BTreeMap<usize, Vec<Option<TyKind>>>,
    pub closures: BTreeMap<usize, Vec<Option<TyKind>>>,
    pub enums: BTreeMap<usize, BTreeMap<usize, Vec<Option<TyKind>>>>,
    pub arrays: BTreeMap<(usize, usize), Option<TyKind>>,
    /// Raw-pointer interner id -> pointee type, carried on each body.
    pub pointees: BTreeMap<usize, TyKind>,
    /// Function-pointer signature id -> (params, return), carried on each
    /// body; the emitter spells `ae_fn_t{id}` from it.
    pub fn_sigs: BTreeMap<usize, (Vec<TyKind>, TyKind)>,
    /// Struct id -> (user drop function, its return type, live-flag member).
    pub drop_fns: BTreeMap<usize, (DefId, TyKind, String)>,
    /// Vector element interner id -> element type, carried on each body.
    pub vector_elems: BTreeMap<usize, TyKind>,
    /// Slice element interner id -> element type, carried on each body.
    pub slice_elems: BTreeMap<usize, TyKind>,
    /// Map interner id -> (key type, value type), carried on each body.
    pub map_types: BTreeMap<usize, (TyKind, TyKind)>,
    /// Closure/function DefId -> captured slot types, for spawn entry
    /// trampolines (the lifted function's parameters are those slots).
    pub task_spawns: BTreeMap<DefId, Vec<TyKind>>,
    /// Thread spawn sites: closure DefId -> (captured slots, return type).
    pub thread_spawns: BTreeMap<DefId, (Vec<TyKind>, TyKind)>,
}

impl ModuleTypes {
    pub fn collect(bodies: &[Body]) -> Self {
        let mut types = Self::default();

        // Register all declared nominal/array types first.
        for body in bodies {
            for decl in &body.locals {
                types.register_ty(decl.ty);
            }
            for (id, pointee) in &body.pointees {
                types.pointees.entry(*id).or_insert(*pointee);
            }
            for (sig, signature) in &body.fn_sigs {
                types.fn_sigs.entry(*sig).or_insert_with(|| signature.clone());
            }
            for (sid, drop) in &body.drop_fns {
                types.drop_fns.entry(*sid).or_insert_with(|| drop.clone());
            }
            for (elem_id, elem_ty) in &body.vector_elems {
                types.vector_elems.entry(*elem_id).or_insert(*elem_ty);
            }
            for (elem_id, elem_ty) in &body.slice_elems {
                types.slice_elems.entry(*elem_id).or_insert(*elem_ty);
            }
            for (map_id, pair) in &body.map_types {
                types.map_types.entry(*map_id).or_insert(*pair);
            }
        }

        // Fixed-point pass (up to 4 iterations) to propagate field/element types
        // from Aggregates and Place projections.
        for _ in 0..4 {
            for body in bodies {
                types.scan_body(body);
            }
        }

        // A thread trampoline stores the closure's return value in its
        // context; the return type comes from the closure body.
        let rets: BTreeMap<DefId, TyKind> = bodies
            .iter()
            .filter_map(|body| {
                body.def.map(|def| {
                    (
                        def,
                        body.locals
                            .first()
                            .map(|local| local.ty)
                            .unwrap_or(TyKind::Unit),
                    )
                })
            })
            .collect();
        for (def, (_, ret)) in types.thread_spawns.iter_mut() {
            if let Some(found) = rets.get(def) {
                *ret = *found;
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
        // Seed every declared variant from the body's layout table: a match
        // arm binds a payload the body may never construct, and the union
        // typedef still needs that member.
        for (id, variants) in &body.enum_layouts {
            let entry = self.enums.entry(*id).or_default();
            for (variant, fields) in variants.iter().enumerate() {
                let slots = entry.entry(variant).or_default();
                let field_tys: Vec<Option<TyKind>> = fields.iter().map(|ty| Some(*ty)).collect();
                ensure_field_slots(slots, &field_tys);
            }
        }
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
            if let Some(term) = &bb.terminator
                && let TerminatorKind::Call { func, args, .. } = &term.kind
            {
                self.scan_task_spawn(body, func, args);
            }
        }
    }

    /// Records a spawn site's callable and captured slot types, so the module
    /// can emit one context struct and entry trampoline per lifted function.
    fn scan_task_spawn(&mut self, body: &Body, func: &Operand, args: &[Operand]) {
        let Operand::Constant(c) = func else { return };
        let ConstValue::FnDef(def_id) = &c.value else { return };
        let Some(name) = body.intrinsics.get(def_id) else { return };
        if name == "thread_spawn" {
            // Thread spawn passes the callable last: [slots..., callable].
            let Some(Operand::Constant(callable)) = args.last() else { return };
            let ConstValue::FnDef(closure_id) = &callable.value else { return };
            let slots: Vec<TyKind> = args[..args.len().saturating_sub(1)]
                .iter()
                .filter_map(|op| self.infer_operand_ty(body, op))
                .collect();
            self.thread_spawns
                .entry(*closure_id)
                .or_insert((slots, TyKind::Unit));
            return;
        }
        if !matches!(name.as_str(), "task_spawn" | "scope_spawn" | "pool_spawn") {
            return;
        }
        let Some(Operand::Constant(callable)) = args.get(1) else { return };
        let ConstValue::FnDef(closure_id) = &callable.value else { return };
        let slots: Vec<TyKind> = args[2..]
            .iter()
            .filter_map(|op| self.infer_operand_ty(body, op))
            .collect();
        self.task_spawns.entry(*closure_id).or_insert(slots);
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
                    AggregateKind::Array(elem_ty, elem_id) => {
                        // The literal names its interned element id, so two
                        // arrays of the same length never share a slot.
                        self.register_ty(*elem_ty);
                        self.arrays.insert((*elem_id, ops.len()), Some(*elem_ty));
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

    /// The element type of a vector interner id, when the body carried it.
    pub fn vector_elem(&self, id: usize) -> Option<TyKind> {
        self.vector_elems.get(&id).copied()
    }

    /// The element type of a slice interner id, when the body carried it.
    pub fn slice_elem(&self, id: usize) -> Option<TyKind> {
        self.slice_elems.get(&id).copied()
    }

    /// The (key, value) types of a map interner id, when the body carried
    /// them.
    pub fn map_types(&self, id: usize) -> Option<(TyKind, TyKind)> {
        self.map_types.get(&id).copied()
    }

    /// The C expression naming a per-element drop thunk (`NULL` for a Copy
    /// element kind). The thunks release the value in place so the vector can
    /// drop every element it owns.
    pub fn elem_drop_expr(&self, ty: TyKind) -> String {
        match ty {
            TyKind::String => "ae_str_release_elem".to_owned(),
            TyKind::Struct(id) if self.structs.contains_key(&id) => {
                format!("ae_rel_struct_elem_{id}")
            }
            TyKind::Enum(_, id) if self.enums.contains_key(&id) => {
                format!("ae_rel_enum_elem_{id}")
            }
            TyKind::Vector(_) => "ae_vec_release_elem".to_owned(),
            _ => "NULL".to_owned(),
        }
    }

    /// Element kinds whose storage the vector runtime can release. Containers
    /// that need their own model (maps) and inline arrays fail closed until
    /// their stage lands.
    pub fn elem_supported(&self, ty: TyKind) -> bool {
        !matches!(
            ty,
            TyKind::MapOf(_)
                | TyKind::ArrayOf(_, _)
                | TyKind::Thread(_)
                | TyKind::Guard
                | TyKind::TaskScope
                | TyKind::Shared(_)
                | TyKind::Mutex
                | TyKind::Chan
                | TyKind::Exec
                | TyKind::Pool
                | TyKind::Atomic
        )
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
                AggregateKind::Array(_elem_ty, elem_id) => {
                    Some(TyKind::ArrayOf(*elem_id, ops.len()))
                }
                AggregateKind::Tuple => None,
            },
            Rvalue::Len(_) => Some(TyKind::Int(IntTy::Usize)),
            Rvalue::Discriminant(_) => Some(TyKind::Int(IntTy::I64)),
            Rvalue::Ref { .. } | Rvalue::AddressOf(_) => Some(TyKind::Ptr(0)),
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
                    TyKind::SliceMut(elem_id) => {
                        cur_ty = self.slice_elem(elem_id)?;
                    }
                    _ => return None,
                },
                ProjectionElem::Deref => {
                    // A reference deref names the borrowed type; a raw
                    // pointer's pointee is not carried here yet.
                    cur_ty = match cur_ty {
                        TyKind::StructRef(sid) | TyKind::StructMut(sid) => TyKind::Struct(sid),
                        TyKind::VectorRef(id) => TyKind::Vector(id),
                        TyKind::Ptr(id) | TyKind::PtrMut(id) => *self.pointees.get(&id)?,
                        _ => return None,
                    };
                }
            }
        }
        Some(cur_ty)
    }

    pub fn emit_typedefs(
        &self,
        out: &mut String,
        def_map: &BTreeMap<DefId, String>,
    ) -> Result<(), CodegenError> {
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
        for &sig in self.fn_sigs.keys() {
            all_keys.insert(TypeDefKey::FnSig(sig));
        }

        let mut visited = BTreeSet::new();
        let mut visiting = BTreeSet::new();

        for key in all_keys {
            self.emit_typedef_topo(key, out, &mut visited, &mut visiting, def_map)?;
        }
        if !visited.is_empty() {
            out.push('\n');
        }
        Ok(())
    }

    /// Emits one context struct and entry trampoline per lifted function a
    /// spawn site names. The trampoline calls the lifted function (its
    /// parameters are the captured slots), so this runs after the function
    /// forward declarations.
    pub fn emit_task_entries(
        &self,
        out: &mut String,
        def_map: &BTreeMap<DefId, String>,
    ) -> Result<(), CodegenError> {
        for (closure_id, slots) in &self.task_spawns {
            let id = closure_id.0;
            let name = def_map.get(closure_id).ok_or_else(|| CodegenError::UnresolvedCall {
                body_name: "<spawn>".to_owned(),
                def_id: *closure_id,
            })?;
            let ctx = format!("ae_task_ctx_{id}");
            if slots.is_empty() {
                let _ = writeln!(out, "typedef struct {{ uint8_t _empty; }} {ctx};");
            } else {
                let _ = writeln!(out, "typedef struct {{");
                for (i, ty) in slots.iter().enumerate() {
                    let _ = writeln!(out, "    {} f{i};", c_type_name(*ty));
                }
                let _ = writeln!(out, "}} {ctx};");
            }
            let args = (0..slots.len())
                .map(|i| format!("c->f{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "static void ae_task_entry_{id}(void *raw) {{");
            let _ = writeln!(out, "    {ctx} *c = ({ctx} *)raw;");
            let _ = writeln!(out, "    (void)c;");
            let _ = writeln!(out, "    {name}({args});");
            let _ = writeln!(out, "}}");
        }
        if !self.task_spawns.is_empty() {
            out.push('\n');
        }
        for (closure_id, (slots, ret)) in &self.thread_spawns {
            let id = closure_id.0;
            let name = def_map.get(closure_id).ok_or_else(|| CodegenError::UnresolvedCall {
                body_name: "<spawn>".to_owned(),
                def_id: *closure_id,
            })?;
            let ctx = format!("ae_thread_ctx_{id}");
            let ret_c = c_type_name(*ret);
            let _ = writeln!(out, "typedef struct {{");
            let _ = writeln!(out, "    {ret_c} ret;");
            for (i, ty) in slots.iter().enumerate() {
                let _ = writeln!(out, "    {} f{i};", c_type_name(*ty));
            }
            let _ = writeln!(out, "}} {ctx};");
            let args = (0..slots.len())
                .map(|i| format!("c->f{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "static void ae_thread_entry_{id}(void *raw) {{");
            let _ = writeln!(out, "    {ctx} *c = ({ctx} *)raw;");
            let _ = writeln!(out, "    c->ret = {name}({args});");
            let _ = writeln!(out, "}}");
        }
        if !self.thread_spawns.is_empty() {
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
        def_map: &BTreeMap<DefId, String>,
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
            self.emit_typedef_topo(dep, out, visited, visiting, def_map)?;
        }

        visiting.remove(&key);
        visited.insert(key);
        self.emit_single_typedef(key, out);
        // Ownership helpers follow each definition in the same topological
        // order: a helper may call only helpers of types it depends on, and
        // those were emitted before this type was complete.
        self.emit_ownership_helpers(key, out, def_map);
        Ok(())
    }

    /// Emits `ae_rel_*` (release the owned value in this slot) and
    /// `ae_take_*` (copy the value out and leave the slot empty) for one
    /// nominal type. A move takes the value; a drop releases it. Both are
    /// idempotent on an empty slot, which is what lets conservative release
    /// lists coexist with moves.
    fn emit_ownership_helpers(
        &self,
        key: TypeDefKey,
        out: &mut String,
        def_map: &BTreeMap<DefId, String>,
    ) {
        match key {
            TypeDefKey::Struct(id) => {
                let fields = self.structs.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                self.emit_record_helpers(out, "struct", id, fields, def_map);
                // Element thunk: lets a vector of this struct release one
                // element in place through the same helper.
                let _ = writeln!(
                    out,
                    "static inline void ae_rel_struct_elem_{id}(void *p) {{ ae_rel_struct_{id}((ae_struct_{id} *)p); }}"
                );
            }
            TypeDefKey::Closure(id) => {
                let fields = self.closures.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                self.emit_record_helpers(out, "closure", id, fields, def_map);
            }
            TypeDefKey::Enum(id) => {
                self.emit_enum_helpers(out, id);
                let _ = writeln!(
                    out,
                    "static inline void ae_rel_enum_elem_{id}(void *p) {{ ae_rel_enum_{id}((ae_enum_{id} *)p); }}"
                );
            }
            // Arrays are Copy-shaped in the pure backend for now; owned
            // element arrays arrive with the container stage.
            TypeDefKey::Array(_, _) | TypeDefKey::FnSig(_) => {}
        }
    }

    fn emit_record_helpers(
        &self,
        out: &mut String,
        kind: &str,
        id: usize,
        fields: &[Option<TyKind>],
        def_map: &BTreeMap<DefId, String>,
    ) {
        let ty = format!("ae_{kind}_{id}");
        let _ = writeln!(out, "static inline void ae_rel_{kind}_{id}({ty} *v) {{");
        // A Copy-only record has no release statements; keep the parameter
        // named for the signature but silent for -Wunused-parameter.
        let _ = writeln!(out, "    (void)v;");
        if kind == "struct"
            && let Some((drop_id, drop_ret, live)) = self.drop_fns.get(&id)
        {
            // One user drop call per value: a moved-from slot is zeroed (the
            // flag is false) and a second release returns immediately.
            let drop_name = def_map.get(drop_id).map(String::as_str).unwrap_or("");
            let _ = writeln!(out, "    if (!v->{live}) return;");
            let _ = writeln!(out, "    v->{live} = false;");
            if !drop_name.is_empty() {
                let _ = writeln!(
                    out,
                    "    extern {} {drop_name}(void *);",
                    c_type_name(*drop_ret)
                );
                let _ = writeln!(out, "    {drop_name}((void *)v);");
            }
        }
        for (idx, field) in fields.iter().enumerate() {
            if let Some(field_ty) = field {
                self.emit_release_field(out, &format!("v->f{idx}"), *field_ty);
            }
        }
        let _ = writeln!(out, "}}");
        let _ = writeln!(out, "static inline {ty} ae_take_{kind}_{id}({ty} *v) {{");
        let _ = writeln!(out, "    {ty} out = *v;");
        let _ = writeln!(out, "    *v = ({ty}){{0}};");
        let _ = writeln!(out, "    return out;");
        let _ = writeln!(out, "}}");
    }

    fn emit_enum_helpers(&self, out: &mut String, id: usize) {
        let _ = writeln!(out, "static inline void ae_rel_enum_{id}(ae_enum_{id} *v) {{");
        let _ = writeln!(out, "    switch (v->tag) {{");
        if let Some(variants) = self.enums.get(&id) {
            for (variant, fields) in variants {
                if !fields.iter().any(|f| f.is_some()) {
                    continue;
                }
                let _ = writeln!(out, "        case {variant}:");
                for (idx, field) in fields.iter().enumerate() {
                    if let Some(field_ty) = field {
                        self.emit_release_field(out, &format!("v->u.v{variant}.f{idx}"), *field_ty);
                    }
                }
                let _ = writeln!(out, "            break;");
            }
        }
        let _ = writeln!(out, "    }}");
        let _ = writeln!(out, "}}");
        let _ = writeln!(out, "static inline ae_enum_{id} ae_take_enum_{id}(ae_enum_{id} *v) {{");
        let _ = writeln!(out, "    ae_enum_{id} out = *v;");
        let _ = writeln!(out, "    *v = (ae_enum_{id}){{0}};");
        let _ = writeln!(out, "    return out;");
        let _ = writeln!(out, "}}");
    }

    fn emit_release_field(&self, out: &mut String, accessor: &str, field_ty: TyKind) {
        match field_ty {
            TyKind::String => {
                let _ = writeln!(out, "    ae_str_release(&{accessor});");
            }
            TyKind::Struct(id) if self.structs.contains_key(&id) => {
                let _ = writeln!(out, "    ae_rel_struct_{id}(&{accessor});");
            }
            TyKind::Enum(_, id) if self.enums.contains_key(&id) => {
                let _ = writeln!(out, "    ae_rel_enum_{id}(&{accessor});");
            }
            TyKind::Vector(_) => {
                let _ = writeln!(out, "    ae_vec_release(&{accessor});");
            }
            TyKind::MapOf(_) => {
                let _ = writeln!(out, "    ae_map_release(&{accessor});");
            }
            _ => {}
        }
    }

    fn direct_deps(&self, key: TypeDefKey) -> Vec<TypeDefKey> {
        let mut deps = Vec::new();
        let mut push_ty = |ty: TyKind| match ty {
            TyKind::Struct(id) => deps.push(TypeDefKey::Struct(id)),
            TyKind::Closure(id) => deps.push(TypeDefKey::Closure(id)),
            TyKind::Enum(_, id) => deps.push(TypeDefKey::Enum(id)),
            TyKind::ArrayOf(elem_id, len) => deps.push(TypeDefKey::Array(elem_id, len)),
            // A struct field may hold a function pointer; its typedef must
            // exist before the struct that names it.
            TyKind::Fn(sig) => deps.push(TypeDefKey::FnSig(sig)),
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
            TypeDefKey::FnSig(sig) => {
                if let Some((params, ret)) = self.fn_sigs.get(&sig) {
                    for ty in params {
                        push_ty(*ty);
                    }
                    push_ty(*ret);
                }
            }
        }
        deps
    }

    fn emit_single_typedef(&self, key: TypeDefKey, out: &mut String) {
        match key {
            TypeDefKey::Struct(id) => {
                let fields = self.structs.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                let live = self.drop_fns.get(&id).map(|(_, _, live)| live.as_str());
                emit_struct_typedef(out, &format!("ae_struct_{id}"), fields, live);
            }
            TypeDefKey::Closure(id) => {
                let fields = self.closures.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                emit_struct_typedef(out, &format!("ae_closure_{id}"), fields, None);
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
            TypeDefKey::FnSig(sig) => {
                let (params, ret) = self
                    .fn_sigs
                    .get(&sig)
                    .cloned()
                    .unwrap_or_else(|| (Vec::new(), TyKind::Unit));
                let params = if params.is_empty() {
                    "void".to_owned()
                } else {
                    params
                        .iter()
                        .map(|ty| c_type_name(*ty))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(out, "typedef {} (*ae_fn_t{sig})({params});", c_type_name(ret));
            }
        }
    }
}

fn emit_struct_typedef(
    out: &mut String,
    name: &str,
    fields: &[Option<TyKind>],
    live: Option<&str>,
) {
    if fields.is_empty() && live.is_none() {
        let _ = writeln!(out, "typedef struct {{ uint8_t _empty; }} {name};");
        return;
    }
    let _ = writeln!(out, "typedef struct {{");
    for (idx, f_ty) in fields.iter().enumerate() {
        let c_ty = c_type_name(f_ty.unwrap_or(TyKind::Int(IntTy::I64)));
        let _ = writeln!(out, "    {c_ty} f{idx};");
    }
    if let Some(live) = live {
        let _ = writeln!(out, "    bool {live};");
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
