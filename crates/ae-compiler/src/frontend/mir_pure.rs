//! Pure CFG-based MIR lowering (P3.1 Slice 2).
//!
//! Lowers `ae-compiler`'s CFG (`cfg::build_cfg`) into `ae_mir::Body` with
//! zero AST `Statement` or `Expr` nodes retained. Short-circuit `&&` and `||`
//! expressions are lowered into explicit basic blocks and `SwitchInt` branches.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

use ae_mir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, BorrowKind, CastKind,
    ConstFloat, ConstValue, Constant, LocalDecl, LocalId, LocalKind, Mutability, Operand, Place,
    ProjectionElem, Rvalue, Statement as MirStatement, StatementKind, SwitchTargets, Terminator,
    TerminatorKind, UnOp, format_body, validate,
};
use ae_ty::{DefId, FloatTy, IntTy, TyKind};

use super::cfg::{CfgFunction, CfgTerminator, build_cfg};
use super::mir::expr_type;
use super::{
    Argument, CompileContext, Expr, Function, Kind, MapKind, MapPlace, PackageImports, Printed,
    Program, Statement, StructDef, Type, loader,
};

/// Lowers every function in the AE program at `path` into validated `ae_mir::Body` graphs.
pub fn lower_path_to_pure_mir(path: &Path) -> Result<Vec<Body>, String> {
    lower_path_with_imports(path, &PackageImports::new())
}

/// Lowers, optimizes, validates, and borrow-checks every function in the AE program at `path`.
pub fn compile_path_pure_mir_bodies(path: &Path) -> Result<Vec<Body>, String> {
    let mut bodies = lower_path_to_pure_mir(path)?;
    for body in &mut bodies {
        ae_mir::opt::optimize_body(body);
        if let Err(errors) = validate(body) {
            let formatted = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("MIR validation failed for `{}`: {formatted}", body.name));
        }
        if let Err(errors) = ae_mir::borrowck::check_body(body) {
            let formatted = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("MIR borrow check failed for `{}`: {formatted}", body.name));
        }
    }
    Ok(bodies)
}

/// Compiles the AE program at `path` end-to-end through pure MIR optimization,
/// validation, borrow checking, and C11 code generation.
pub fn compile_path_pure_mir_to_c(
    path: &Path,
    config: &ae_codegen_c::EmitConfig,
) -> Result<String, String> {
    let bodies = compile_path_pure_mir_bodies(path)?;
    ae_codegen_c::emit_module_c(&bodies, config).map_err(|e| e.to_string())
}

/// Lowers and deterministically formats every function in the AE program at `path`.
pub fn format_pure_mir_path(path: &Path) -> Result<String, String> {
    let bodies = lower_path_to_pure_mir(path)?;
    let mut out = String::new();
    for (idx, body) in bodies.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(&format_body(body));
        out.push('\n');
    }
    Ok(out)
}

pub(super) fn lower_path_with_imports(
    path: &Path,
    imports: &PackageImports,
) -> Result<Vec<Body>, String> {
    let program = loader::load_program_with_imports(path, imports)?;
    lower_program_to_pure_mir(&program)
}

pub(super) fn lower_program_to_pure_mir(program: &Program) -> Result<Vec<Body>, String> {
    let enum_layouts: BTreeMap<usize, Vec<Vec<TyKind>>> = program
        .enums
        .iter()
        .enumerate()
        .map(|(id, def)| {
            (
                id,
                def.variants
                    .iter()
                    .map(|(_, fields)| fields.clone())
                    .collect(),
            )
        })
        .collect();
    let drop_fns: BTreeMap<usize, (DefId, TyKind, String)> = program
        .structs
        .iter()
        .enumerate()
        .filter_map(|(sid, def)| {
            def.drop_fn.map(|id| {
                let ret = if def.drop_returns_unit {
                    TyKind::Unit
                } else {
                    TyKind::Int(IntTy::I64)
                };
                // The release helper needs a flag that no declared field can
                // shadow; choose it here, where field names are known.
                let mut live = "ae_live".to_owned();
                while def.fields.iter().any(|(name, _)| name == &live) {
                    live.push('_');
                }
                (sid, (DefId(id as u32), ret, live))
            })
        })
        .collect();
    let copy_structs: BTreeSet<usize> = program
        .structs
        .iter()
        .enumerate()
        .filter(|(_, def)| def.is_copy)
        .map(|(sid, _)| sid)
        .collect();
    let copy_enums: BTreeSet<usize> = program
        .enums
        .iter()
        .enumerate()
        .filter(|(_, def)| def.is_copy)
        .map(|(eid, _)| eid)
        .collect();
    let mut bodies = Vec::new();
    for file in &program.files {
        for function in file {
            let body = lower_function_to_pure_mir(
                function,
                &program.structs,
                &program.ctx,
                &enum_layouts,
                &drop_fns,
                &copy_structs,
                &copy_enums,
            )?;
            bodies.push(body);
        }
    }
    Ok(bodies)
}

pub(super) fn lower_function_to_pure_mir(
    function: &Function,
    structs: &[StructDef],
    ctx: &CompileContext,
    enum_layouts: &BTreeMap<usize, Vec<Vec<TyKind>>>,
    drop_fns: &BTreeMap<usize, (DefId, TyKind, String)>,
    copy_structs: &BTreeSet<usize>,
    copy_enums: &BTreeSet<usize>,
) -> Result<Body, String> {
    let cfg = build_cfg(function, ctx);
    let mut builder = PureMirBuilder::new(
        function,
        &cfg,
        structs,
        ctx,
        enum_layouts,
        drop_fns,
        copy_structs,
        copy_enums,
    );
    builder.lower_cfg(&cfg);
    if !builder.unsupported.is_empty() {
        return Err(format!(
            "pure MIR lowering cannot handle `{}`: {}",
            builder.name,
            builder.unsupported.join("; ")
        ));
    }
    let body = builder.finish();
    if let Err(errors) = validate(&body) {
        let formatted = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("MIR validation failed for `{}`: {formatted}", body.name));
    }
    Ok(body)
}

struct PureMirBuilder<'a> {
    name: String,
    def: Option<DefId>,
    arg_count: usize,
    locals: Vec<LocalDecl>,
    blocks: Vec<BasicBlockData>,
    var_map: HashMap<usize, (LocalId, TyKind)>,
    cfg_entry_bbs: Vec<BasicBlockId>,
    cur_bb: BasicBlockId,
    structs: &'a [StructDef],
    ctx: &'a CompileContext,
    /// Synthetic intrinsic `DefId`s registered while lowering, carried on
    /// the body so a backend can emit the builtin itself.
    intrinsics: BTreeMap<DefId, String>,
    /// Signatures seen on function-value constants (`FnRef`, fn-pointer
    /// arguments), merged with the locals' signatures at `finish`.
    fn_sigs_seen: BTreeMap<usize, (Vec<TyKind>, TyKind)>,
    /// Declared extern callees seen while lowering, carried on the body so
    /// the backend can name the symbol instead of reporting a raw DefId.
    externs: BTreeMap<DefId, String>,
    enum_layouts: &'a BTreeMap<usize, Vec<Vec<TyKind>>>,
    drop_fns: &'a BTreeMap<usize, (DefId, TyKind, String)>,
    copy_structs: &'a BTreeSet<usize>,
    copy_enums: &'a BTreeSet<usize>,
    /// Shapes this function uses that the pure pipeline cannot lower yet;
    /// a non-empty list turns the whole lowering into an error (fail closed)
    /// instead of emitting ill-formed MIR.
    unsupported: Vec<String>,
}

impl<'a> PureMirBuilder<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        function: &Function,
        cfg: &CfgFunction,
        structs: &'a [StructDef],
        ctx: &'a CompileContext,
        enum_layouts: &'a BTreeMap<usize, Vec<Vec<TyKind>>>,
        drop_fns: &'a BTreeMap<usize, (DefId, TyKind, String)>,
        copy_structs: &'a BTreeSet<usize>,
        copy_enums: &'a BTreeSet<usize>,
    ) -> Self {
        let name = if function.is_main {
            "main".to_owned()
        } else {
            format!("fn#{}", function.id)
        };
        let mut locals = Vec::with_capacity(1 + cfg.params.len());
        locals.push(LocalDecl::new(
            function.returns,
            Mutability::Mut,
            LocalKind::ReturnPlace,
        ));

        let mut var_map = HashMap::new();
        for &(param_id, param_ty) in &cfg.params {
            let local_id = LocalId(locals.len() as u32);
            locals.push(
                LocalDecl::new(param_ty, Mutability::Mut, LocalKind::Arg)
                    .with_name(format!("v{param_id}")),
            );
            var_map.insert(param_id, (local_id, param_ty));
        }

        let mut blocks = Vec::with_capacity(cfg.blocks.len().max(1));
        let mut cfg_entry_bbs = Vec::with_capacity(cfg.blocks.len());
        for i in 0..cfg.blocks.len() {
            cfg_entry_bbs.push(BasicBlockId(i as u32));
            blocks.push(BasicBlockData::default());
        }
        if blocks.is_empty() {
            blocks.push(BasicBlockData::new(
                Vec::new(),
                Some(Terminator::new(TerminatorKind::Return)),
            ));
        }

        Self {
            name,
            def: Some(DefId(function.id as u32)),
            arg_count: cfg.params.len(),
            locals,
            blocks,
            var_map,
            cfg_entry_bbs,
            cur_bb: BasicBlockId::ENTRY,
            structs,
            ctx,
            intrinsics: BTreeMap::new(),
            fn_sigs_seen: BTreeMap::new(),
            externs: BTreeMap::new(),
            enum_layouts,
            drop_fns,
            copy_structs,
            copy_enums,
            unsupported: Vec::new(),
        }
    }

    /// Copy types keep the source binding usable after a by-value use, so a
    /// moving position lowers to a plain copy for them.
    fn is_copy_ty(&self, ty: TyKind) -> bool {
        match ty {
            TyKind::Struct(id) => self.copy_structs.contains(&id),
            TyKind::Enum(_, id) => self.copy_enums.contains(&id),
            other => !other.owned(),
        }
    }

    fn new_block(&mut self) -> BasicBlockId {
        let id = BasicBlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlockData::default());
        id
    }

    fn fresh_temp(&mut self, ty: TyKind) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals
            .push(LocalDecl::new(ty, Mutability::Mut, LocalKind::Temp));
        id
    }

    fn ensure_var(&mut self, var_id: usize, ty: TyKind) -> (LocalId, TyKind) {
        if let Some(&existing) = self.var_map.get(&var_id) {
            return existing;
        }
        let local_id = LocalId(self.locals.len() as u32);
        let label = if var_id >= usize::MAX - 1_000_000 {
            format!("syn#{}", usize::MAX - var_id)
        } else {
            format!("v{var_id}")
        };
        self.locals.push(
            LocalDecl::new(ty, Mutability::Mut, LocalKind::Var).with_name(label),
        );
        self.var_map.insert(var_id, (local_id, ty));
        (local_id, ty)
    }

    fn push_stmt(&mut self, kind: StatementKind) {
        self.blocks[self.cur_bb.index()]
            .statements
            .push(MirStatement::new(kind));
    }

    fn set_terminator(&mut self, kind: TerminatorKind) {
        self.blocks[self.cur_bb.index()].terminator = Some(Terminator::new(kind));
    }

    fn assign_to_temp(&mut self, ty: TyKind, rvalue: Rvalue) -> LocalId {
        let temp = self.fresh_temp(ty);
        self.push_stmt(StatementKind::Assign(Place::from_local(temp), rvalue));
        temp
    }

    fn lower_cfg(&mut self, cfg: &CfgFunction) {
        for (idx, cfg_block) in cfg.blocks.iter().enumerate() {
            self.cur_bb = self.cfg_entry_bbs[idx];
            for stmt in &cfg_block.statements {
                self.lower_statement(stmt);
            }
            self.lower_terminator(&cfg_block.terminator);
        }
    }

    fn finish(mut self) -> Body {
        // Prune unreachable blocks and compact BasicBlockIds densely from bb0.
        let n = self.blocks.len();
        let mut reachable = vec![false; n];
        let mut queue = VecDeque::new();
        if n > 0 {
            reachable[0] = true;
            queue.push_back(BasicBlockId::ENTRY);
        }
        while let Some(bb) = queue.pop_front() {
            if let Some(term) = &self.blocks[bb.index()].terminator {
                for succ in term.kind.successors() {
                    if succ.index() < n && !reachable[succ.index()] {
                        reachable[succ.index()] = true;
                        queue.push_back(succ);
                    }
                }
            }
        }

        let mut remap = vec![BasicBlockId::ENTRY; n];
        let mut compact_blocks = Vec::new();
        for (old_idx, &is_reach) in reachable.iter().enumerate() {
            if is_reach {
                remap[old_idx] = BasicBlockId(compact_blocks.len() as u32);
                let mut bb = std::mem::take(&mut self.blocks[old_idx]);
                if bb.terminator.is_none() {
                    bb.terminator = Some(Terminator::new(TerminatorKind::Unreachable));
                }
                compact_blocks.push(bb);
            }
        }

        for bb in &mut compact_blocks {
            if let Some(term) = &mut bb.terminator {
                remap_terminator(&mut term.kind, &remap);
            }
        }

        // Every pointer type the body can touch (locals, nested container
        // elements, struct fields, enum payloads) contributes its pointee,
        // and every function-pointer type contributes its signature, so a
        // standalone backend can type a dereference and an indirect call.
        let mut pointees = BTreeMap::new();
        let mut fn_sigs = BTreeMap::new();
        let mut visited_structs = BTreeSet::new();
        for decl in &self.locals {
            collect_pointees(
                decl.ty,
                self.structs,
                self.enum_layouts,
                self.ctx,
                &mut visited_structs,
                &mut pointees,
            );
            collect_fn_sigs(
                decl.ty,
                self.structs,
                self.enum_layouts,
                self.ctx,
                &mut BTreeSet::new(),
                &mut fn_sigs,
            );
        }
        for (sig, signature) in &self.fn_sigs_seen {
            fn_sigs.entry(*sig).or_insert_with(|| signature.clone());
        }
        Body {
            name: self.name,
            def: self.def,
            arg_count: self.arg_count,
            locals: self.locals,
            basic_blocks: compact_blocks,
            intrinsics: self.intrinsics,
            pointees,
            fn_sigs,
            externs: self.externs,
            enum_layouts: self.enum_layouts.clone(),
            drop_fns: self.drop_fns.clone(),
            copy_structs: self.copy_structs.clone(),
            copy_enums: self.copy_enums.clone(),
            span: None,
        }
    }

    fn lower_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Declare(id, expr) => {
                let ty = expr_type(expr, self.ctx);
                let (local, _) = self.ensure_var(*id, ty);
                let rvalue = self.lower_moving_expr(expr);
                self.assign_checked(Place::from_local(local), ty, rvalue, ty);
            }
            Statement::Assign(id, expr, release_old) => {
                let rhs_ty = expr_type(expr, self.ctx);
                let (local, var_ty) = self.ensure_var(*id, rhs_ty);
                if *release_old && var_ty.owned() {
                    self.push_stmt(StatementKind::Drop(Place::from_local(local)));
                }
                let rvalue = self.lower_moving_expr(expr);
                self.assign_checked(Place::from_local(local), var_ty, rvalue, rhs_ty);
            }
            Statement::Array(id, values, elem_ty) => {
                let arr_id = self.ctx.find_array_elem(*elem_ty).unwrap_or(0);
                let arr_ty = TyKind::ArrayOf(arr_id, values.len());
                let (local, _) = self.ensure_var(*id, arr_ty);
                let ops = values
                    .iter()
                    .map(|v| self.lower_operand_moving(v))
                    .collect();
                self.push_stmt(StatementKind::Assign(
                    Place::from_local(local),
                    Rvalue::Aggregate(AggregateKind::Array(*elem_ty, arr_id), ops),
                ));
            }
            Statement::Store(id, len, index, value, _, _, elem_ty) => {
                let arr_id = self.ctx.find_array_elem(*elem_ty).unwrap_or(0);
                let arr_ty = TyKind::ArrayOf(arr_id, *len);
                let (local, _) = self.ensure_var(*id, arr_ty);
                let idx_local = self.lower_index_local(index);
                let val_op = self.lower_operand_moving(value);
                let place = Place::from_local(local).project(ProjectionElem::Index(idx_local));
                self.push_stmt(StatementKind::Assign(place, Rvalue::Use(val_op)));
            }
            Statement::FieldStore(id, path, _fty, value, base_ty) => {
                let (local, _) = self.ensure_var(*id, *base_ty);
                let place = self.build_field_path_place(local, *base_ty, path);
                let val_op = self.lower_operand_moving(value);
                self.push_stmt(StatementKind::Assign(place, Rvalue::Use(val_op)));
            }
            Statement::BindPayload(bid, subject, path, pty) => {
                let (dest_local, dest_ty) = self.ensure_var(*bid, *pty);
                let (subj_local, _) = self.ensure_var(*subject, *pty);
                let mut src_place = Place::from_local(subj_local);
                for &(variant, field_idx) in path {
                    src_place.projection.push(ProjectionElem::Downcast(variant));
                    src_place.projection.push(ProjectionElem::Field(field_idx));
                }
                // A match binding of an owned payload is a borrow in the
                // language: the enum keeps ownership, so the binding must
                // not release the payload a second time.
                let rvalue = if *pty == TyKind::String {
                    let view = self.emit_intrinsic_call(
                        "string_view",
                        vec![Operand::Copy(src_place)],
                        TyKind::String,
                    );
                    Rvalue::Use(Operand::Copy(Place::from_local(view)))
                } else {
                    Rvalue::Use(Operand::Copy(src_place))
                };
                self.push_stmt(StatementKind::Assign(
                    Place::from_local(dest_local),
                    rvalue,
                ));
                let _ = dest_ty;
            }
            Statement::TakePropagationPayload(subject, ty, _fields, binding) => {
                let (subj_local, _) = self.ensure_var(*subject, *ty);
                if let Some((bid, pty)) = binding {
                    let (dest_local, _) = self.ensure_var(*bid, *pty);
                    let src_place = Place::from_local(subj_local)
                        .project(ProjectionElem::Downcast(0))
                        .project(ProjectionElem::Field(0));
                    let rvalue = if *pty == TyKind::String {
                        let view = self.emit_intrinsic_call(
                            "string_view",
                            vec![Operand::Copy(src_place)],
                            TyKind::String,
                        );
                        Rvalue::Use(Operand::Copy(Place::from_local(view)))
                    } else {
                        Rvalue::Use(Operand::Copy(src_place))
                    };
                    self.push_stmt(StatementKind::Assign(
                        Place::from_local(dest_local),
                        rvalue,
                    ));
                }
            }
            Statement::Frees(list) => {
                for &(var_id, ty) in list {
                    let (local, local_ty) = self.ensure_var(var_id, ty);
                    if local_ty.owned() {
                        self.push_stmt(StatementKind::Drop(Place::from_local(local)));
                    }
                }
            }
            Statement::Assert(cond, ..) => {
                let cond_op = self.lower_operand(cond);
                let next_bb = self.new_block();
                self.set_terminator(TerminatorKind::Assert {
                    cond: cond_op,
                    expected: true,
                    msg: AssertMessage::Custom("assert".to_owned()),
                    target: next_bb,
                });
                self.cur_bb = next_bb;
            }
            Statement::Print(printed) => {
                let (arg, owned_temp) = match printed {
                    // A variable or place read prints a borrowed value; any
                    // other owned expression materialized a temporary that
                    // this statement alone owns, so it is released right
                    // after the print.
                    Printed::Expr(e) => self.lower_readonly_operand(e),
                    Printed::Text(s) => (
                        Operand::Constant(Constant::new(TyKind::String, ConstValue::String(s.clone()))),
                        None,
                    ),
                };
                self.emit_intrinsic_call("print", vec![arg], TyKind::Unit);
                if let Some(local) = owned_temp {
                    self.push_stmt(StatementKind::Drop(Place::from_local(local)));
                }
            }
            Statement::StrVecPush(id, value, ..) => {
                let vec_ty = vector_ty(self.ctx, Type::String, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call(
                    "vec_push",
                    vec![Operand::Copy(Place::from_local(local)), val_op],
                    TyKind::Unit,
                );
            }
            Statement::GVecPush(id, value, elem_ty, ..) => {
                let vec_ty = vector_ty(self.ctx, *elem_ty, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call(
                    "vec_push",
                    vec![Operand::Copy(Place::from_local(local)), val_op],
                    TyKind::Unit,
                );
            }
            Statement::StrVecPop(id, ..) => {
                let vec_ty = vector_ty(self.ctx, Type::String, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                self.emit_intrinsic_call(
                    "vec_pop",
                    vec![Operand::Copy(Place::from_local(local))],
                    TyKind::Unit,
                );
            }
            Statement::GVecPop(id, elem_ty, ..) => {
                let vec_ty = vector_ty(self.ctx, *elem_ty, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                self.emit_intrinsic_call(
                    "vec_pop",
                    vec![Operand::Copy(Place::from_local(local))],
                    TyKind::Unit,
                );
            }
            Statement::StrVecStore(id, index, value, ..) => {
                let vec_ty = vector_ty(self.ctx, Type::String, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let idx_op = self.lower_operand(index);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call(
                    "vec_store",
                    vec![Operand::Copy(Place::from_local(local)), idx_op, val_op],
                    TyKind::Unit,
                );
            }
            Statement::GVecStore(id, index, value, elem_ty, ..) => {
                let vec_ty = vector_ty(self.ctx, *elem_ty, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let idx_op = self.lower_operand(index);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call(
                    "vec_store",
                    vec![Operand::Copy(Place::from_local(local)), idx_op, val_op],
                    TyKind::Unit,
                );
            }
            Statement::SliceStore(id, index, value, elem_ty, ..) => {
                let slice_ty = TyKind::SliceMut(self.ctx.find_slice_elem(*elem_ty).unwrap_or(0));
                let (local, _) = self.ensure_var(*id, slice_ty);
                let idx_local = self.lower_index_local(index);
                let val_op = self.lower_operand_moving(value);
                let place = Place::from_local(local).project(ProjectionElem::Index(idx_local));
                self.push_stmt(StatementKind::Assign(place, Rvalue::Use(val_op)));
            }
            Statement::Sort(id, is_str, ..) => {
                let elem = if *is_str { Type::String } else { Type::Int(IntTy::I64) };
                let vec_ty = vector_ty(self.ctx, elem, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                self.emit_intrinsic_call(
                    if *is_str { "sort_strings" } else { "sort" },
                    vec![Operand::Copy(Place::from_local(local))],
                    TyKind::Unit,
                );
            }
            Statement::Reverse(id, is_str, ..) => {
                let elem = if *is_str { Type::String } else { Type::Int(IntTy::I64) };
                let vec_ty = vector_ty(self.ctx, elem, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                self.emit_intrinsic_call(
                    if *is_str { "reverse_strings" } else { "reverse" },
                    vec![Operand::Copy(Place::from_local(local))],
                    TyKind::Unit,
                );
            }
            Statement::VecPushField(id, path, base_ty, value, elem_ty, ..) => {
                let recv = self.field_vector_place(*id, path, *base_ty, *elem_ty);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call("vec_push", vec![Operand::Copy(recv), val_op], TyKind::Unit);
            }
            Statement::VecPopField(id, path, base_ty, elem_ty, ..) => {
                let recv = self.field_vector_place(*id, path, *base_ty, *elem_ty);
                self.emit_intrinsic_call("vec_pop", vec![Operand::Copy(recv)], TyKind::Unit);
            }
            Statement::VecStoreField(id, path, base_ty, index, value, elem_ty, ..) => {
                let recv = self.field_vector_place(*id, path, *base_ty, *elem_ty);
                let idx_op = self.lower_operand(index);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call(
                    "vec_store",
                    vec![Operand::Copy(recv), idx_op, val_op],
                    TyKind::Unit,
                );
            }
            Statement::VecSortField(id, path, base_ty, elem_ty, reverse, ..) => {
                let recv = self.field_vector_place(*id, path, *base_ty, *elem_ty);
                let name = if *reverse { "reverse_field" } else { "sort_field" };
                self.emit_intrinsic_call(name, vec![Operand::Copy(recv)], TyKind::Unit);
            }
            Statement::MapPut(place, key, value, ..) => {
                let map_op = self.lower_map_place(place);
                let key_op = self.lower_operand(key);
                let val_op = self.lower_operand(value);
                self.emit_intrinsic_call("map_put", vec![map_op, key_op, val_op], TyKind::Unit);
            }
            // A token cursor needs a runtime cursor model (reset/advance) the pure
            // pipeline does not implement yet. Fail closed: emitting the
            // current best-effort shape leaves the iteration local
            // uninitialized, which the borrow checker rightly rejects.
            Statement::ForTokens(..) | Statement::ResetTokens(..) | Statement::AdvanceTokens(..) => {
                let message = "token cursors (for-in over a tokenize result) are not supported by the pure MIR pipeline yet";
                if !self.unsupported.iter().any(|existing| existing == message) {
                    self.unsupported.push(message.to_owned());
                }
            }
            Statement::PtrStore(id, value, ..) => {
                let val_ty = expr_type(value, self.ctx);
                let ptr_ty = self
                    .ctx
                    .find_pointee(val_ty)
                    .map(TyKind::PtrMut)
                    .unwrap_or(TyKind::PtrMut(0));
                let (local, _) = self.ensure_var(*id, ptr_ty);
                let val_op = self.lower_operand(value);
                let place = Place::from_local(local).project(ProjectionElem::Deref);
                self.push_stmt(StatementKind::Assign(place, Rvalue::Use(val_op)));
            }
            Statement::AtomicStore(handle, value) => {
                let h = self.lower_operand(handle);
                let v = self.lower_operand(value);
                self.emit_intrinsic_call("atomic_store", vec![h, v], TyKind::Unit);
            }
            Statement::SharedSet(handle, value, ..) => {
                let h = self.lower_operand(handle);
                let v = self.lower_operand(value);
                self.emit_intrinsic_call("shared_set", vec![h, v], TyKind::Unit);
            }
            Statement::ChanClose(handle) => {
                let h = self.lower_operand(handle);
                self.emit_intrinsic_call("chan_close", vec![h], TyKind::Unit);
            }
            Statement::TaskSpawn(ex, slots, callable, ..) => {
                let mut ops = vec![self.lower_operand(ex), self.lower_operand(callable)];
                ops.extend(slots.iter().map(|s| self.lower_operand(s)));
                self.emit_intrinsic_call("task_spawn", ops, TyKind::Unit);
            }
            Statement::TaskYield => {
                self.emit_intrinsic_call("task_yield", Vec::new(), TyKind::Unit);
            }
            Statement::ScopeSpawn(sc, slots, callable, ..) => {
                let mut ops = vec![self.lower_operand(sc), self.lower_operand(callable)];
                ops.extend(slots.iter().map(|s| self.lower_operand(s)));
                self.emit_intrinsic_call("scope_spawn", ops, TyKind::Unit);
            }
            Statement::PoolSpawn(p, slots, callable, ..) => {
                let mut ops = vec![self.lower_operand(p), self.lower_operand(callable)];
                ops.extend(slots.iter().map(|s| self.lower_operand(s)));
                self.emit_intrinsic_call("pool_spawn", ops, TyKind::Unit);
            }
            // Control-flow statements are already lowered by `cfg::build_cfg`.
            Statement::If(..)
            | Statement::While(..)
            | Statement::For(..)
            | Statement::Match(..)
            | Statement::Propagate(..)
            | Statement::Return(..)
            | Statement::Exit(..)
            | Statement::Break
            | Statement::Continue(..)
            | Statement::Unsafe(..) => {
                self.push_stmt(StatementKind::Nop);
            }
        }
    }

    fn lower_terminator(&mut self, term: &CfgTerminator) {
        match term {
            CfgTerminator::Goto(target) => {
                let target_bb = self.cfg_entry_bbs[*target];
                self.set_terminator(TerminatorKind::Goto { target: target_bb });
            }
            CfgTerminator::Branch(cond, yes, no) => {
                let yes_bb = self.cfg_entry_bbs[*yes];
                let no_bb = self.cfg_entry_bbs[*no];
                self.lower_branch(cond, yes_bb, no_bb);
            }
            CfgTerminator::Switch(subject, path, _layout, targets, default) => {
                let (subj_local, _) = self.ensure_var(*subject, TyKind::Enum("", 0));
                let mut place = Place::from_local(subj_local);
                for &(variant, field_idx) in path {
                    place.projection.push(ProjectionElem::Downcast(variant));
                    place.projection.push(ProjectionElem::Field(field_idx));
                }
                let discr_local = self.assign_to_temp(
                    TyKind::Int(IntTy::I64),
                    Rvalue::Discriminant(place),
                );
                let mut cases: Vec<(u128, BasicBlockId)> = targets
                    .iter()
                    .map(|&(v, bb)| (v as u128, self.cfg_entry_bbs[bb]))
                    .collect();
                let otherwise = if let Some(def_bb) = default {
                    self.cfg_entry_bbs[*def_bb]
                } else if let Some((_, last_bb)) = cases.pop() {
                    last_bb
                } else {
                    self.set_terminator(TerminatorKind::Unreachable);
                    return;
                };
                self.set_terminator(TerminatorKind::SwitchInt {
                    discr: Operand::Copy(Place::from_local(discr_local)),
                    targets: SwitchTargets::new(cases, otherwise),
                });
            }
            CfgTerminator::Return(expr) => {
                let val_ty = expr_type(expr, self.ctx);
                // The returned expression is a moving position: a bare
                // binding is taken, leaving its slot empty for the release
                // list below.
                let val_op = self.lower_operand_moving(expr);
                let ret_ty = self.locals[0].ty;
                // The parser's scope releases ride the return (the legacy
                // backends run them *after* the value is copied out). The CFG
                // appends them as trailing `Drop` statements, so the return
                // assignment is inserted before that run: a drop of the
                // returned binding must not precede the copy that moves it.
                let insert_at = {
                    let statements = &self.blocks[self.cur_bb.index()].statements;
                    let mut at = statements.len();
                    while at > 0 && matches!(statements[at - 1].kind, StatementKind::Drop(_)) {
                        at -= 1;
                    }
                    at
                };
                let mark = self.blocks[self.cur_bb.index()].statements.len();
                self.assign_checked(
                    Place::from_local(LocalId::RETURN_PLACE),
                    ret_ty,
                    Rvalue::Use(val_op),
                    val_ty,
                );
                let tail = self.blocks[self.cur_bb.index()].statements.split_off(mark);
                self.blocks[self.cur_bb.index()].statements.splice(insert_at..insert_at, tail);
                self.set_terminator(TerminatorKind::Return);
            }
            CfgTerminator::Exit(expr) => {
                let code_op = self.lower_operand(expr);
                let dest = self.fresh_temp(TyKind::Unit);
                let func = self.intrinsic_fn("exit");
                self.set_terminator(TerminatorKind::Call {
                    func,
                    args: vec![code_op],
                    destination: Place::from_local(dest),
                    target: None,
                });
            }
            CfgTerminator::Unreachable => {
                // `cfg::build_cfg` leaves `main`'s implicit fallthrough as `CfgTerminator::Unreachable`
                // (meaning implicit exit-0); initialize `_0` and return normally instead of trapping.
                if self.name == "main" {
                    let ret_ty = self.locals[0].ty;
                    let default_val = match ret_ty {
                        TyKind::Int(_) => ConstValue::Int(0),
                        _ => ConstValue::Unit,
                    };
                    let val_ty = match ret_ty {
                        TyKind::Int(_) => ret_ty,
                        _ => TyKind::Unit,
                    };
                    self.push_stmt(StatementKind::Assign(
                        Place::from_local(LocalId::RETURN_PLACE),
                        Rvalue::Use(Operand::Constant(Constant::new(val_ty, default_val))),
                    ));
                    self.set_terminator(TerminatorKind::Return);
                } else {
                    self.set_terminator(TerminatorKind::Unreachable);
                }
            }
        }
    }

    fn lower_branch(&mut self, cond: &Expr, yes_bb: BasicBlockId, no_bb: BasicBlockId) {
        match cond {
            Expr::Binary(lhs, Kind::And, rhs, ..) => {
                let rhs_bb = self.new_block();
                self.lower_branch(lhs, rhs_bb, no_bb);
                self.cur_bb = rhs_bb;
                self.lower_branch(rhs, yes_bb, no_bb);
            }
            Expr::Binary(lhs, Kind::Or, rhs, ..) => {
                let rhs_bb = self.new_block();
                self.lower_branch(lhs, yes_bb, rhs_bb);
                self.cur_bb = rhs_bb;
                self.lower_branch(rhs, yes_bb, no_bb);
            }
            Expr::Not(inner) => {
                self.lower_branch(inner, no_bb, yes_bb);
            }
            other => {
                let discr = self.lower_operand(other);
                self.set_terminator(TerminatorKind::SwitchInt {
                    discr,
                    targets: SwitchTargets::if_bool(yes_bb, no_bb),
                });
            }
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Rvalue {
        match expr {
            Expr::Number(n) => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::Int(IntTy::I64),
                ConstValue::Int(*n as i128),
            ))),
            Expr::FloatLit(x) => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::Float(FloatTy::F64),
                ConstValue::Float(ConstFloat::from_f64(*x)),
            ))),
            Expr::BoolLit(b) => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::Bool,
                ConstValue::Bool(*b),
            ))),
            Expr::CharLit(c) => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::Char,
                ConstValue::Char(*c),
            ))),
            Expr::Unit => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::Unit,
                ConstValue::Unit,
            ))),
            Expr::StrLit(s, ..) => Rvalue::Use(Operand::Constant(Constant::new(
                TyKind::String,
                ConstValue::String(s.clone()),
            ))),
            Expr::Variable(id, ty, ..) => {
                let (local, _) = self.ensure_var(*id, *ty);
                Rvalue::Use(Operand::Copy(Place::from_local(local)))
            }
            Expr::Field(base, fname, _fty) => {
                let place = self.lower_field_place(base, fname);
                Rvalue::Use(Operand::Copy(place))
            }
            Expr::Binary(lhs, op @ (Kind::And | Kind::Or), rhs, ..) => {
                let result = self.fresh_temp(TyKind::Bool);
                let lhs_op = self.lower_operand(lhs);
                let rhs_bb = self.new_block();
                let short_bb = self.new_block();
                let join_bb = self.new_block();

                let (true_bb, false_bb, short_val) = if matches!(op, Kind::And) {
                    (rhs_bb, short_bb, false)
                } else {
                    (short_bb, rhs_bb, true)
                };

                self.set_terminator(TerminatorKind::SwitchInt {
                    discr: lhs_op,
                    targets: SwitchTargets::if_bool(true_bb, false_bb),
                });

                self.cur_bb = short_bb;
                self.push_stmt(StatementKind::Assign(
                    Place::from_local(result),
                    Rvalue::Use(Operand::Constant(Constant::new(
                        TyKind::Bool,
                        ConstValue::Bool(short_val),
                    ))),
                ));
                self.set_terminator(TerminatorKind::Goto { target: join_bb });

                self.cur_bb = rhs_bb;
                let rhs_op = self.lower_operand(rhs);
                self.push_stmt(StatementKind::Assign(
                    Place::from_local(result),
                    Rvalue::Use(rhs_op),
                ));
                self.set_terminator(TerminatorKind::Goto { target: join_bb });

                self.cur_bb = join_bb;
                Rvalue::Use(Operand::Copy(Place::from_local(result)))
            }
            Expr::StrEq(a, b, negated) => {
                // A comparison only reads: a temporary operand (a
                // concatenation, a call result) is released after the
                // comparison, and a variable/field stays untouched.
                let (a_op, a_temp) = self.lower_readonly_operand(a);
                let (b_op, b_temp) = self.lower_readonly_operand(b);
                let name = if *negated { "str_ne" } else { "str_eq" };
                let dest = self.emit_intrinsic_call(name, vec![a_op, b_op], TyKind::Bool);
                for temp in [a_temp, b_temp].into_iter().flatten() {
                    self.push_stmt(StatementKind::Drop(Place::from_local(temp)));
                }
                Rvalue::Use(Operand::Copy(Place::from_local(dest)))
            }
            Expr::Binary(lhs, op, rhs, _, _, ty) => {
                if *ty == TyKind::String && matches!(op, Kind::Plus) {
                    // Concatenation is a moving position for owned operands.
                    let lhs_op = self.lower_operand_moving(lhs);
                    let rhs_op = self.lower_operand_moving(rhs);
                    let res = self.emit_intrinsic_call(
                        "str_concat",
                        vec![lhs_op, rhs_op],
                        TyKind::String,
                    );
                    return Rvalue::Use(Operand::Copy(Place::from_local(res)));
                }
                let lhs_op = self.lower_operand(lhs);
                let rhs_op = self.lower_operand(rhs);
                let bin_op = map_bin_op(op);
                Rvalue::BinaryOp(bin_op, lhs_op, rhs_op)
            }
            Expr::Neg(inner, ..) => {
                let op = self.lower_operand(inner);
                Rvalue::UnaryOp(UnOp::Neg, op)
            }
            Expr::Not(inner) => {
                let op = self.lower_operand(inner);
                Rvalue::UnaryOp(UnOp::Not, op)
            }
            Expr::ScalarCast(inner, from, to, ..) => {
                let op = self.lower_operand(inner);
                Rvalue::Cast(classify_cast(*from, *to), op, *to)
            }
            Expr::ToF64(inner, ..) => {
                let from = expr_type(inner, self.ctx);
                let op = self.lower_operand(inner);
                Rvalue::Cast(classify_cast(from, TyKind::Float(FloatTy::F64)), op, TyKind::Float(FloatTy::F64))
            }
            Expr::ToI64(inner, ..) => {
                let from = expr_type(inner, self.ctx);
                let op = self.lower_operand(inner);
                Rvalue::Cast(classify_cast(from, TyKind::Int(IntTy::I64)), op, TyKind::Int(IntTy::I64))
            }
            Expr::StructLit(sid, values) => {
                let ops = values
                    .iter()
                    .map(|v| self.lower_operand_moving(v))
                    .collect();
                Rvalue::Aggregate(AggregateKind::Struct(*sid), ops)
            }
            Expr::EnumLit(_name, eid, vidx, values) => {
                let ops = values
                    .iter()
                    .map(|v| self.lower_operand_moving(v))
                    .collect();
                Rvalue::Aggregate(AggregateKind::Enum(*eid, *vidx), ops)
            }
            Expr::Len(inner, ..) => {
                let place = self.lower_place(inner);
                Rvalue::Len(place)
            }
            Expr::BorrowAs(inner, mutable, ..) => {
                let place = self.lower_place(inner);
                // `&v as *const T` names an address, not a borrow (the
                // expression's type is the raw pointer); a real `&T` keeps
                // its loan (ADR-0237).
                if matches!(expr_type(expr, self.ctx), TyKind::Ptr(_) | TyKind::PtrMut(_)) {
                    Rvalue::AddressOf(place)
                } else {
                    let borrow_kind = if *mutable {
                        BorrowKind::Mut
                    } else {
                        BorrowKind::Shared
                    };
                    Rvalue::Ref { borrow_kind, place }
                }
            }
            Expr::DerefPtr(inner, ..) => {
                let place = self.lower_place(inner).project(ProjectionElem::Deref);
                Rvalue::Use(Operand::Copy(place))
            }
            Expr::Index(id, len, index, _, _, elem_ty) => {
                let arr_id = self.ctx.find_array_elem(*elem_ty).unwrap_or(0);
                let arr_ty = TyKind::ArrayOf(arr_id, *len);
                let (local, _) = self.ensure_var(*id, arr_ty);
                let idx_local = self.lower_index_local(index);
                let place = Place::from_local(local).project(ProjectionElem::Index(idx_local));
                Rvalue::Use(Operand::Copy(place))
            }
            Expr::FnRef(id, sig) => {
                self.record_fn_sig(*sig);
                Rvalue::Use(Operand::Constant(Constant::new(
                    TyKind::Fn(*sig),
                    ConstValue::FnDef(DefId(*id as u32)),
                )))
            }
            Expr::ClosureLit(cid) => {
                let fn_id = self.ctx.closure_fn_id(*cid);
                Rvalue::Use(Operand::Constant(Constant::new(
                    TyKind::Closure(*cid),
                    ConstValue::FnDef(DefId(fn_id as u32)),
                )))
            }
            Expr::Call(fn_id, args, ret) => {
                if let Some(name) = self.ctx.extern_name(*fn_id) {
                    self.externs.insert(DefId(*fn_id as u32), name);
                }
                let arg_ops = self.lower_args(args);
                let dest = self.fresh_temp(*ret);
                let next_bb = self.new_block();
                self.set_terminator(TerminatorKind::Call {
                    func: Operand::Constant(Constant::new(
                        TyKind::Fn(*fn_id),
                        ConstValue::FnDef(DefId(*fn_id as u32)),
                    )),
                    args: arg_ops,
                    destination: Place::from_local(dest),
                    target: Some(next_bb),
                });
                self.cur_bb = next_bb;
                Rvalue::Use(Operand::Copy(Place::from_local(dest)))
            }
            // A call through a function-typed binding: the slot holds the
            // pointer, so the terminator takes the local as its callee.
            Expr::FnCall(slot, args, ret, _) => {
                let (local, _) = self.ensure_var(*slot, TyKind::Fn(0));
                let arg_ops = self.lower_args(args);
                let dest = self.fresh_temp(*ret);
                let next_bb = self.new_block();
                self.set_terminator(TerminatorKind::Call {
                    func: Operand::Copy(Place::from_local(local)),
                    args: arg_ops,
                    destination: Place::from_local(dest),
                    target: Some(next_bb),
                });
                self.cur_bb = next_bb;
                Rvalue::Use(Operand::Copy(Place::from_local(dest)))
            }
            Expr::FnExprCall(callee, args, ret, _) => {
                let callee_op = self.lower_operand(callee);
                let arg_ops = self.lower_args(args);
                let dest = self.fresh_temp(*ret);
                let next_bb = self.new_block();
                self.set_terminator(TerminatorKind::Call {
                    func: callee_op,
                    args: arg_ops,
                    destination: Place::from_local(dest),
                    target: Some(next_bb),
                });
                self.cur_bb = next_bb;
                Rvalue::Use(Operand::Copy(Place::from_local(dest)))
            }
            other => {
                let ret_ty = expr_type(other, self.ctx);
                let (name, ops) = self.lower_expr_intrinsic_args(other);
                let dest = self.emit_intrinsic_call(name, ops, ret_ty);
                Rvalue::Use(Operand::Copy(Place::from_local(dest)))
            }
        }
    }

    fn lower_operand(&mut self, expr: &Expr) -> Operand {
        match expr {
            Expr::Number(n) => Operand::Constant(Constant::new(
                TyKind::Int(IntTy::I64),
                ConstValue::Int(*n as i128),
            )),
            Expr::FloatLit(x) => Operand::Constant(Constant::new(
                TyKind::Float(FloatTy::F64),
                ConstValue::Float(ConstFloat::from_f64(*x)),
            )),
            Expr::BoolLit(b) => Operand::Constant(Constant::new(
                TyKind::Bool,
                ConstValue::Bool(*b),
            )),
            Expr::CharLit(c) => Operand::Constant(Constant::new(
                TyKind::Char,
                ConstValue::Char(*c),
            )),
            Expr::Unit => Operand::Constant(Constant::new(TyKind::Unit, ConstValue::Unit)),
            Expr::StrLit(s, ..) => Operand::Constant(Constant::new(
                TyKind::String,
                ConstValue::String(s.clone()),
            )),
            Expr::Variable(id, ty, ..) => {
                let (local, _) = self.ensure_var(*id, *ty);
                Operand::Copy(Place::from_local(local))
            }
            other => {
                let ty = expr_type(other, self.ctx);
                let rvalue = self.lower_expr(other);
                let temp = self.assign_to_temp(ty, rvalue);
                Operand::Copy(Place::from_local(temp))
            }
        }
    }

    /// Lower an expression in a moving position. A bare owned variable is
    /// consumed (the emitter leaves its slot empty, so a conservative release
    /// of that slot frees nothing); a field or element read is a borrow and
    /// stays a copy; a private temporary is transferred rather than copied.
    fn lower_operand_moving(&mut self, expr: &Expr) -> Operand {
        match expr {
            Expr::Variable(id, ty, ..) => {
                let (local, _) = self.ensure_var(*id, *ty);
                let place = Place::from_local(local);
                if ty.owned() && !self.is_copy_ty(*ty) {
                    Operand::Move(place)
                } else {
                    Operand::Copy(place)
                }
            }
            Expr::Field(..)
            | Expr::Index(..)
            | Expr::VecIndex(..)
            | Expr::StrVecIndex(..)
            | Expr::DerefPtr(..) => self.lower_operand(expr),
            other => {
                let ty = expr_type(other, self.ctx);
                let rvalue = self.lower_expr(other);
                let temp = self.assign_to_temp(ty, rvalue);
                let place = Place::from_local(temp);
                if ty.owned() && !self.is_copy_ty(ty) {
                    Operand::Move(place)
                } else {
                    Operand::Copy(place)
                }
            }
        }
    }

    /// The root of a `let`/assignment/return: a bare owned binding moves,
    /// every composite expression lowers normally (its own sub-expressions
    /// carry the moving positions).
    fn lower_moving_expr(&mut self, expr: &Expr) -> Rvalue {
        match expr {
            Expr::Variable(..) | Expr::Field(..) | Expr::Index(..) | Expr::DerefPtr(..) => {
                Rvalue::Use(self.lower_operand_moving(expr))
            }
            other => self.lower_expr(other),
        }
    }

    /// Lower an operand that a read-only consumer (print, string comparison)
    /// merely reads: a variable or place is borrowed, and an owned temporary
    /// is reported so the consumer can release it after the read.
    fn lower_readonly_operand(&mut self, expr: &Expr) -> (Operand, Option<LocalId>) {
        let ty = expr_type(expr, self.ctx);
        let place_read = matches!(
            expr,
            Expr::Variable(..)
                | Expr::Field(..)
                | Expr::Index(..)
                | Expr::VecIndex(..)
                | Expr::StrVecIndex(..)
                | Expr::DerefPtr(..)
        );
        if place_read || !ty.owned() || self.is_copy_ty(ty) {
            return (self.lower_operand(expr), None);
        }
        let rvalue = self.lower_expr(expr);
        let temp = self.assign_to_temp(ty, rvalue);
        (Operand::Copy(Place::from_local(temp)), Some(temp))
    }

    fn lower_place(&mut self, expr: &Expr) -> Place {
        match expr {
            Expr::Variable(id, ty, ..) => {
                let (local, _) = self.ensure_var(*id, *ty);
                Place::from_local(local)
            }
            Expr::Field(base, fname, _) => self.lower_field_place(base, fname),
            Expr::DerefPtr(inner, ..) => self.lower_place(inner).project(ProjectionElem::Deref),
            other => {
                let ty = expr_type(other, self.ctx);
                let rvalue = self.lower_expr(other);
                let temp = self.assign_to_temp(ty, rvalue);
                Place::from_local(temp)
            }
        }
    }

    fn lower_field_place(&mut self, base: &Expr, fname: &str) -> Place {
        let base_ty = expr_type(base, self.ctx);
        let mut place = self.lower_place(base);
        match base_ty {
            TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
                place.projection.push(ProjectionElem::Deref);
                let idx = self.resolve_field_index(sid, fname);
                place.projection.push(ProjectionElem::Field(idx));
            }
            TyKind::Struct(sid) => {
                let idx = self.resolve_field_index(sid, fname);
                place.projection.push(ProjectionElem::Field(idx));
            }
            _ => {
                place.projection.push(ProjectionElem::Field(0));
            }
        }
        place
    }

    fn build_field_path_place(
        &self,
        root: LocalId,
        base_ty: TyKind,
        path: &[(String, Type)],
    ) -> Place {
        let mut place = Place::from_local(root);
        let mut cur_ty = base_ty;
        for (fname, step_ty) in path {
            match cur_ty {
                TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
                    place.projection.push(ProjectionElem::Deref);
                    place
                        .projection
                        .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                }
                TyKind::Struct(sid) => {
                    place
                        .projection
                        .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                }
                _ => {
                    place.projection.push(ProjectionElem::Field(0));
                }
            }
            cur_ty = *step_ty;
        }
        place
    }

    fn field_vector_place(
        &mut self,
        id: usize,
        path: &[(String, Type)],
        base_ty: Type,
        _elem_ty: Type,
    ) -> Place {
        let (local, _) = self.ensure_var(id, base_ty);
        self.build_field_path_place(local, base_ty, path)
    }

    fn resolve_field_index(&self, sid: usize, fname: &str) -> usize {
        self.structs
            .get(sid)
            .and_then(|def| def.fields.iter().position(|(n, _)| n == fname))
            .unwrap_or(0)
    }

    fn lower_index_local(&mut self, index: &Expr) -> LocalId {
        match index {
            Expr::Variable(id, ty, ..) if matches!(ty, TyKind::Int(_)) => {
                self.ensure_var(*id, *ty).0
            }
            other => {
                let op = self.lower_operand(other);
                self.assign_to_temp(TyKind::Int(IntTy::I64), Rvalue::Use(op))
            }
        }
    }

    fn lower_args(&mut self, args: &[Argument]) -> Vec<Operand> {
        args.iter()
            .map(|arg| match arg {
                // A by-value argument is a moving position: the callee owns it.
                Argument::Value(e) => self.lower_operand_moving(e),
                Argument::View(e) => {
                    let op = self.lower_operand(e);
                    if expr_type(e, self.ctx) == TyKind::StrView {
                        op
                    } else {
                        let temp = self.emit_intrinsic_call("string_view", vec![op], TyKind::StrView);
                        Operand::Copy(Place::from_local(temp))
                    }
                }
                Argument::Borrow(slot, take_address) => {
                    let ty = self
                        .var_map
                        .get(slot)
                        .map(|(_, t)| *t)
                        .unwrap_or(TyKind::Int(IntTy::I64));
                    let (local, _) = self.ensure_var(*slot, ty);
                    let place = Place::from_local(local);
                    if *take_address {
                        let ptr_ty = self
                            .ctx
                            .find_pointee(ty)
                            .map(TyKind::PtrMut)
                            .unwrap_or(TyKind::PtrMut(0));
                        let temp = self.assign_to_temp(
                            ptr_ty,
                            Rvalue::Ref {
                                borrow_kind: BorrowKind::Shared,
                                place,
                            },
                        );
                        Operand::Copy(Place::from_local(temp))
                    } else {
                        Operand::Copy(place)
                    }
                }
                Argument::FnPtr(id, sig) => {
                    self.record_fn_sig(*sig);
                    Operand::Constant(Constant::new(
                        TyKind::Fn(*sig),
                        ConstValue::FnDef(DefId(*id as u32)),
                    ))
                }
            })
            .collect()
    }

    fn lower_map_place(&mut self, place: &MapPlace) -> Operand {
        match place {
            MapPlace::Var(id, kind) => {
                let ty = map_kind_ty(kind, self.ctx);
                let (local, _) = self.ensure_var(*id, ty);
                Operand::Copy(Place::from_local(local))
            }
            MapPlace::Field(id, fname, kind) => {
                let ty = map_kind_ty(kind, self.ctx);
                let (local, base_ty) = self.ensure_var(*id, ty);
                let mut p = Place::from_local(local);
                match base_ty {
                    TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
                        p.projection.push(ProjectionElem::Deref);
                        p.projection
                            .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                    }
                    TyKind::Struct(sid) => {
                        p.projection
                            .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                    }
                    _ => p.projection.push(ProjectionElem::Field(0)),
                }
                Operand::Copy(p)
            }
        }
    }

    fn emit_intrinsic_call(
        &mut self,
        name: &'static str,
        args: Vec<Operand>,
        ret_ty: TyKind,
    ) -> LocalId {
        let dest = self.fresh_temp(ret_ty);
        let next_bb = self.new_block();
        let func = self.intrinsic_fn(name);
        self.set_terminator(TerminatorKind::Call {
            func,
            args,
            destination: Place::from_local(dest),
            target: Some(next_bb),
        });
        self.cur_bb = next_bb;
        dest
    }

    fn assign_checked(
        &mut self,
        place: Place,
        expected_ty: TyKind,
        rvalue: Rvalue,
        actual_ty: TyKind,
    ) {
        if expected_ty == actual_ty {
            self.push_stmt(StatementKind::Assign(place, rvalue));
        } else {
            let temp = self.assign_to_temp(actual_ty, rvalue);
            self.push_stmt(StatementKind::Assign(
                place,
                Rvalue::Cast(
                    classify_cast(actual_ty, expected_ty),
                    Operand::Copy(Place::from_local(temp)),
                    expected_ty,
                ),
            ));
        }
    }

    fn lower_expr_intrinsic_args(&mut self, expr: &Expr) -> (&'static str, Vec<Operand>) {
        match expr {
            Expr::VecLit(values, elem_ty, ..) => {
                let _ = elem_ty;
                let ops = values
                    .iter()
                    .map(|v| self.lower_operand_moving(v))
                    .collect();
                ("vec_new", ops)
            }
            Expr::VecIndex(id, index, _, _, borrowed, elem) => {
                let vec_ty = vector_ty(self.ctx, *elem, *borrowed);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let idx_op = self.lower_operand(index);
                ("vec_index", vec![Operand::Copy(Place::from_local(local)), idx_op])
            }
            Expr::StrVecIndex(id, index, _, _, borrowed) => {
                let vec_ty = vector_ty(self.ctx, Type::String, *borrowed);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let idx_op = self.lower_operand(index);
                ("vec_index", vec![Operand::Copy(Place::from_local(local)), idx_op])
            }
            Expr::VecIndexOf(base, index, ..) => {
                let b = self.lower_operand(base);
                let i = self.lower_operand(index);
                ("vec_index_of", vec![b, i])
            }
            Expr::SliceOf(slot, source, elem, ..) => {
                let ty = match *source {
                    Type::Slice(_) => TyKind::Slice(self.ctx.find_slice_elem(*elem).unwrap_or(0)),
                    other => other,
                };
                let (local, _) = self.ensure_var(*slot, ty);
                ("slice_of", vec![Operand::Copy(Place::from_local(local))])
            }
            Expr::SliceIndex(slot, index, _, _, elem) => {
                let ty = TyKind::Slice(self.ctx.find_slice_elem(*elem).unwrap_or(0));
                let (local, _) = self.ensure_var(*slot, ty);
                let idx_op = self.lower_operand(index);
                ("slice_index", vec![Operand::Copy(Place::from_local(local)), idx_op])
            }
            Expr::SliceRange(slot, source, elem, start, end, ..) => {
                let ty = match *source {
                    Type::Slice(_) => TyKind::Slice(self.ctx.find_slice_elem(*elem).unwrap_or(0)),
                    other => other,
                };
                let (local, _) = self.ensure_var(*slot, ty);
                let s = self.lower_operand(start);
                let e = self.lower_operand(end);
                ("slice_range", vec![Operand::Copy(Place::from_local(local)), s, e])
            }
            Expr::SliceView(id, field, start, end, ..) => {
                let base = self.string_base_operand(*id, field);
                let s = self.lower_operand(start);
                let e = self.lower_operand(end);
                ("slice_view", vec![base, s, e])
            }
            Expr::Tokens(id, field, sep, ..) => {
                let base = self.string_base_operand(*id, field);
                let s = self.lower_operand(sep);
                ("tokens", vec![base, s])
            }
            Expr::Join(id, sep, ..) => {
                let vec_ty = vector_ty(self.ctx, Type::String, false);
                let (local, _) = self.ensure_var(*id, vec_ty);
                let s = self.lower_operand(sep);
                ("join", vec![Operand::Copy(Place::from_local(local)), s])
            }
            Expr::Split(base, sep, ..) => {
                let b = self.lower_operand(base);
                let s = self.lower_operand(sep);
                ("split", vec![b, s])
            }
            Expr::Args(..) => ("args", Vec::new()),
            Expr::ReadFile(p, ..) | Expr::TryReadFile(p, ..) => ("read_file", vec![self.lower_operand(p)]),
            Expr::WriteFile(p, t, ..) | Expr::TryWriteFile(p, t, ..) => {
                let p_op = self.lower_operand(p);
                let t_op = self.lower_operand(t);
                ("write_file", vec![p_op, t_op])
            }
            Expr::Bytes(e, ..) => ("bytes", vec![self.lower_operand(e)]),
            Expr::Contains(h, n, ..) => {
                let h_op = self.lower_operand(h);
                let n_op = self.lower_operand(n);
                ("contains", vec![h_op, n_op])
            }
            Expr::Slice(b, s, e, ..) => {
                let b_op = self.lower_operand(b);
                let s_op = self.lower_operand(s);
                let e_op = self.lower_operand(e);
                ("slice", vec![b_op, s_op, e_op])
            }
            Expr::FloatToString(e, ..) => ("float_to_string", vec![self.lower_operand(e)]),
            Expr::ToString(e, ..) => ("to_string", vec![self.lower_operand(e)]),
            Expr::StrEq(a, b, negated) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                (if *negated { "str_ne" } else { "str_eq" }, vec![a_op, b_op])
            }
            Expr::MapNew(..) => ("map_new", Vec::new()),
            Expr::MapGet(p, k, ..) => {
                let p_op = self.lower_map_place(p);
                let k_op = self.lower_operand(k);
                ("map_get", vec![p_op, k_op])
            }
            Expr::MapHas(p, k, ..) => {
                let p_op = self.lower_map_place(p);
                let k_op = self.lower_operand(k);
                ("map_has", vec![p_op, k_op])
            }
            Expr::MapRemove(p, k, ..) => {
                let p_op = self.lower_map_place(p);
                let k_op = self.lower_operand(k);
                ("map_remove", vec![p_op, k_op])
            }
            Expr::MapInc(p, k, v, ..) => {
                let p_op = self.lower_map_place(p);
                let k_op = self.lower_operand(k);
                let v_op = self.lower_operand(v);
                ("map_inc", vec![p_op, k_op, v_op])
            }
            Expr::MapKeys(p, ..) => ("map_keys", vec![self.lower_map_place(p)]),
            Expr::RunCommand(prog, vid, ..) => {
                let prog_op = self.lower_operand(prog);
                let vec_ty = vector_ty(self.ctx, Type::String, false);
                let (local, _) = self.ensure_var(*vid, vec_ty);
                ("run_command", vec![prog_op, Operand::Copy(Place::from_local(local))])
            }
            Expr::Lines(e, ..) => ("lines", vec![self.lower_operand(e)]),
            Expr::CountLinesWith(t, p, ..) => {
                let t_op = self.lower_operand(t);
                let p_op = self.lower_operand(p);
                ("count_lines_with", vec![t_op, p_op])
            }
            Expr::TryListDir(p, ..) => ("list_dir", vec![self.lower_operand(p)]),
            Expr::PathExists(p, ..) => ("path_exists", vec![self.lower_operand(p)]),
            Expr::PathIsDir(p, ..) => ("path_is_dir", vec![self.lower_operand(p)]),
            Expr::ParseInt(t, ..) => ("parse_int", vec![self.lower_operand(t)]),
            Expr::PathJoin(a, b, ..) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("path_join", vec![a_op, b_op])
            }
            Expr::PathName(p, ..) => ("path_name", vec![self.lower_operand(p)]),
            Expr::PathParent(p, ..) => ("path_parent", vec![self.lower_operand(p)]),
            Expr::TryFileInfo(p, ..) => ("file_info", vec![self.lower_operand(p)]),
            Expr::TryCopyFile(a, b, ..) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("copy_file", vec![a_op, b_op])
            }
            Expr::TryRenamePath(a, b, ..) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("rename_path", vec![a_op, b_op])
            }
            Expr::TryRemoveFile(p, ..) => ("remove_file", vec![self.lower_operand(p)]),
            Expr::TryRemoveDir(p, ..) => ("remove_dir", vec![self.lower_operand(p)]),
            Expr::TryCreateDir(p, ..) => ("create_dir", vec![self.lower_operand(p)]),
            Expr::SetCwd(p, ..) => ("set_cwd", vec![self.lower_operand(p)]),
            Expr::Utf8Len(e, ..) => ("utf8_len", vec![self.lower_operand(e)]),
            Expr::Utf8Prefix(e, n, ..) => {
                let e_op = self.lower_operand(e);
                let n_op = self.lower_operand(n);
                ("utf8_prefix", vec![e_op, n_op])
            }
            Expr::Trim(e, ..) => ("trim", vec![self.lower_operand(e)]),
            Expr::StartsWith(a, b) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("starts_with", vec![a_op, b_op])
            }
            Expr::EndsWith(a, b) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("ends_with", vec![a_op, b_op])
            }
            Expr::TryAppendFile(p, t, ..) => {
                let p_op = self.lower_operand(p);
                let t_op = self.lower_operand(t);
                ("append_file", vec![p_op, t_op])
            }
            Expr::Env(n, ..) => ("env", vec![self.lower_operand(n)]),
            Expr::JsonGet(s, k, ..) => {
                let s_op = self.lower_operand(s);
                let k_op = self.lower_operand(k);
                ("json_get", vec![s_op, k_op])
            }
            Expr::JsonInt(s, k, ..) => {
                let s_op = self.lower_operand(s);
                let k_op = self.lower_operand(k);
                ("json_int", vec![s_op, k_op])
            }
            Expr::JsonBool(s, k, ..) => {
                let s_op = self.lower_operand(s);
                let k_op = self.lower_operand(k);
                ("json_bool", vec![s_op, k_op])
            }
            Expr::JsonLen(s, k, ..) => {
                let s_op = self.lower_operand(s);
                let k_op = self.lower_operand(k);
                ("json_len", vec![s_op, k_op])
            }
            Expr::JsonHas(s, k, ..) => {
                let s_op = self.lower_operand(s);
                let k_op = self.lower_operand(k);
                ("json_has", vec![s_op, k_op])
            }
            Expr::Find(t, p, ..) => {
                let t_op = self.lower_operand(t);
                let p_op = self.lower_operand(p);
                ("find", vec![t_op, p_op])
            }
            Expr::ReadStdin(..) => ("read_stdin", Vec::new()),
            Expr::Cwd(..) => ("cwd", Vec::new()),
            Expr::StrVecNew(..) => ("strvec_new", Vec::new()),
            Expr::F64VecNew(..) => ("f64vec_new", Vec::new()),
            Expr::NowMs => ("now_ms", Vec::new()),
            Expr::NowS => ("now_s", Vec::new()),
            Expr::MathCall(index, args, ..) => {
                let name = super::MATH_FNS[*index as usize].0;
                let ops = args.iter().map(|a| self.lower_operand(a)).collect();
                (name, ops)
            }
            Expr::IntMethod(kind, recv, arg, ..) => {
                let mut ops = vec![self.lower_operand(recv)];
                if let Some(arg) = arg {
                    ops.push(self.lower_operand(arg));
                }
                (kind.mir_name(), ops)
            }
            Expr::Spawn(slots, callable, ..) => {
                let mut ops: Vec<Operand> = slots.iter().map(|s| self.lower_operand(s)).collect();
                ops.push(self.lower_operand(callable));
                ("thread_spawn", ops)
            }
            Expr::Wait(handle, ..) => ("thread_wait", vec![self.lower_operand(handle)]),
            Expr::AtomicNew(v) => ("atomic_new", vec![self.lower_operand(v)]),
            Expr::AtomicLoad(h) => ("atomic_load", vec![self.lower_operand(h)]),
            Expr::AtomicAdd(h, v) => {
                let h_op = self.lower_operand(h);
                let v_op = self.lower_operand(v);
                ("atomic_add", vec![h_op, v_op])
            }
            Expr::AtomicCas(h, e, d) => {
                let h_op = self.lower_operand(h);
                let e_op = self.lower_operand(e);
                let d_op = self.lower_operand(d);
                ("atomic_cas", vec![h_op, e_op, d_op])
            }
            Expr::SharedNew(v, _) => ("shared_new", vec![self.lower_operand(v)]),
            Expr::SharedGet(h, _) => ("shared_get", vec![self.lower_operand(h)]),
            Expr::MutexNew => ("mutex_new", Vec::new()),
            Expr::MutexLock(h) => ("mutex_lock", vec![self.lower_operand(h)]),
            Expr::ChanNew(c) => ("chan_new", vec![self.lower_operand(c)]),
            Expr::ChanSend(h, v) => {
                let h_op = self.lower_operand(h);
                let v_op = self.lower_operand(v);
                ("chan_send", vec![h_op, v_op])
            }
            Expr::ChanTrySend(h, v) => {
                let h_op = self.lower_operand(h);
                let v_op = self.lower_operand(v);
                ("chan_try_send", vec![h_op, v_op])
            }
            Expr::ChanRecv(h) => ("chan_recv", vec![self.lower_operand(h)]),
            Expr::ChanSelect(a, b) => {
                let a_op = self.lower_operand(a);
                let b_op = self.lower_operand(b);
                ("chan_select", vec![a_op, b_op])
            }
            Expr::ExecNew => ("exec_new", Vec::new()),
            Expr::ExecRun(h) => ("exec_run", vec![self.lower_operand(h)]),
            Expr::ScopeNew => ("scope_new", Vec::new()),
            Expr::PoolNew(n) => ("pool_new", vec![self.lower_operand(n)]),
            Expr::PoolJoin(h) => ("pool_join", vec![self.lower_operand(h)]),
            Expr::PtrOffset(p, c, bytewise, ..) => {
                let p_op = self.lower_operand(p);
                let c_op = self.lower_operand(c);
                (if *bytewise { "byte_offset" } else { "ptr_offset" }, vec![p_op, c_op])
            }
            Expr::PtrCast(p, mutable, ..) => (
                if *mutable { "ptr_cast_mut" } else { "ptr_cast" },
                vec![self.lower_operand(p)],
            ),
            Expr::PtrDropInPlace(p, ..) => ("ptr_drop_in_place", vec![self.lower_operand(p)]),
            Expr::PtrWrite(p, v, ..) => {
                let p_op = self.lower_operand(p);
                let v_op = self.lower_operand(v);
                ("ptr_write", vec![p_op, v_op])
            }
            // All other expressions are handled directly in `lower_expr`.
            _ => ("unknown_expr", Vec::new()),
        }
    }

    fn string_base_operand(&mut self, id: usize, field: &Option<String>) -> Operand {
        let (local, base_ty) = self.ensure_var(id, TyKind::String);
        let mut place = Place::from_local(local);
        if let Some(fname) = field {
            match base_ty {
                TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
                    place.projection.push(ProjectionElem::Deref);
                    place
                        .projection
                        .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                }
                TyKind::Struct(sid) => {
                    place
                        .projection
                        .push(ProjectionElem::Field(self.resolve_field_index(sid, fname)));
                }
                _ => place.projection.push(ProjectionElem::Field(0)),
            }
        }
        Operand::Copy(place)
    }
}

impl PureMirBuilder<'_> {
    /// The call operand for a builtin intrinsic. The id is a deterministic
    /// FNV-1a 32-bit hash in the high-bit range (never a source fn id); the
    /// name is recorded on the body so a backend emits the builtin.
    fn intrinsic_fn(&mut self, name: &str) -> Operand {
        let mut hash: u32 = 0x811c_9dc5;
        for &b in name.as_bytes() {
            hash ^= b as u32;
            hash = hash.wrapping_mul(0x0100_0193);
        }
        let id = 0x8000_0000 | (hash & 0x7fff_ffff);
        self.intrinsics.insert(DefId(id), name.to_owned());
        Operand::Constant(Constant::new(TyKind::Fn(0), ConstValue::FnDef(DefId(id))))
    }

    /// Record a function-value constant's signature (a `FnRef` names it
    /// directly; the id may be absent from the interner for a placeholder).
    fn record_fn_sig(&mut self, sig: usize) {
        if !self.fn_sigs_seen.contains_key(&sig)
            && let Some(signature) = self.ctx.fn_sig_checked(sig)
        {
            self.fn_sigs_seen.insert(sig, signature);
        }
    }
}

/// Walk `ty` for raw-pointer pointee ids (and the types reachable through
/// containers, struct fields and enum payloads), recording `id -> pointee`.
fn collect_pointees(
    ty: TyKind,
    structs: &[StructDef],
    enum_layouts: &BTreeMap<usize, Vec<Vec<TyKind>>>,
    ctx: &CompileContext,
    visited_structs: &mut BTreeSet<usize>,
    out: &mut BTreeMap<usize, TyKind>,
) {
    match ty {
        TyKind::Ptr(id) | TyKind::PtrMut(id) => {
            if let Some(pointee) = ctx.pointee_checked(id) {
                out.entry(id).or_insert(pointee);
            }
        }
        TyKind::Vector(id) | TyKind::VectorRef(id) => {
            collect_pointees(ctx.vector_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::Slice(id) | TyKind::SliceMut(id) => {
            collect_pointees(ctx.slice_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::ArrayOf(id, _) => {
            collect_pointees(ctx.array_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::MapOf(id) => {
            let (key, value) = ctx.map_types(id);
            collect_pointees(key, structs, enum_layouts, ctx, visited_structs, out);
            collect_pointees(value, structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::Struct(sid) | TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
            if visited_structs.insert(sid) {
                for (_, field) in &structs[sid].fields {
                    collect_pointees(*field, structs, enum_layouts, ctx, visited_structs, out);
                }
            }
        }
        TyKind::Enum(_, eid) => {
            if let Some(variants) = enum_layouts.get(&eid) {
                for fields in variants {
                    for field in fields {
                        collect_pointees(*field, structs, enum_layouts, ctx, visited_structs, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Walk `ty` for `fn(..) -> ..` types and record each signature
/// (`sig id -> (params, return)`) so a backend can spell the pointer and
/// cast an indirect callee.
fn collect_fn_sigs(
    ty: TyKind,
    structs: &[StructDef],
    enum_layouts: &BTreeMap<usize, Vec<Vec<TyKind>>>,
    ctx: &CompileContext,
    visited_structs: &mut BTreeSet<usize>,
    out: &mut BTreeMap<usize, (Vec<TyKind>, TyKind)>,
) {
    match ty {
        TyKind::Fn(sig) => {
            if let Some((params, ret)) = ctx.fn_sig_checked(sig) {
                out.entry(sig).or_insert((params, ret));
            }
        }
        TyKind::Ptr(id) | TyKind::PtrMut(id) => {
            if let Some(pointee) = ctx.pointee_checked(id) {
                collect_fn_sigs(pointee, structs, enum_layouts, ctx, visited_structs, out);
            }
        }
        TyKind::Vector(id) | TyKind::VectorRef(id) => {
            collect_fn_sigs(ctx.vector_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::Slice(id) | TyKind::SliceMut(id) => {
            collect_fn_sigs(ctx.slice_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::ArrayOf(id, _) => {
            collect_fn_sigs(ctx.array_elem(id), structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::MapOf(id) => {
            let (key, value) = ctx.map_types(id);
            collect_fn_sigs(key, structs, enum_layouts, ctx, visited_structs, out);
            collect_fn_sigs(value, structs, enum_layouts, ctx, visited_structs, out);
        }
        TyKind::Struct(sid) | TyKind::StructRef(sid) | TyKind::StructMut(sid) => {
            if visited_structs.insert(sid) {
                for (_, field) in &structs[sid].fields {
                    collect_fn_sigs(*field, structs, enum_layouts, ctx, visited_structs, out);
                }
            }
        }
        TyKind::Enum(_, eid) => {
            if let Some(variants) = enum_layouts.get(&eid) {
                for fields in variants {
                    for field in fields {
                        collect_fn_sigs(*field, structs, enum_layouts, ctx, visited_structs, out);
                    }
                }
            }
        }
        _ => {}
    }
}

fn vector_ty(ctx: &CompileContext, elem: Type, borrowed: bool) -> TyKind {
    let id = ctx.find_vector_elem(elem).unwrap_or(0);
    if borrowed {
        TyKind::VectorRef(id)
    } else {
        TyKind::Vector(id)
    }
}

fn map_kind_ty(kind: &MapKind, ctx: &CompileContext) -> TyKind {
    let value = match kind {
        MapKind::Int => Type::Int(IntTy::I64),
        MapKind::Float => Type::Float(FloatTy::F64),
    };
    TyKind::MapOf(ctx.find_map_types(Type::String, value).unwrap_or(0))
}

fn map_bin_op(op: &Kind) -> BinOp {
    match op {
        Kind::Plus => BinOp::Add,
        Kind::Minus => BinOp::Sub,
        Kind::Star => BinOp::Mul,
        Kind::Slash => BinOp::Div,
        Kind::Percent => BinOp::Rem,
        Kind::Caret => BinOp::BitXor,
        Kind::Amp => BinOp::BitAnd,
        Kind::Pipe => BinOp::BitOr,
        Kind::Shl => BinOp::Shl,
        Kind::Shr => BinOp::Shr,
        Kind::EqEq => BinOp::Eq,
        Kind::Less => BinOp::Lt,
        Kind::LessEq => BinOp::Le,
        Kind::NotEq => BinOp::Ne,
        Kind::GreaterEq => BinOp::Ge,
        Kind::Greater => BinOp::Gt,
        _ => BinOp::Add,
    }
}

fn classify_cast(from: TyKind, to: TyKind) -> CastKind {
    match (from, to) {
        (TyKind::Int(_), TyKind::Int(_)) => CastKind::IntToInt,
        (TyKind::Int(_), TyKind::Float(_)) => CastKind::IntToFloat,
        (TyKind::Float(_), TyKind::Int(_)) => CastKind::FloatToInt,
        (TyKind::Float(_), TyKind::Float(_)) => CastKind::FloatToFloat,
        (TyKind::Ptr(_) | TyKind::PtrMut(_), TyKind::Ptr(_) | TyKind::PtrMut(_)) => {
            CastKind::PtrToPtr
        }
        _ => CastKind::Misc,
    }
}

fn remap_terminator(kind: &mut TerminatorKind, remap: &[BasicBlockId]) {
    match kind {
        TerminatorKind::Goto { target } => {
            *target = remap[target.index()];
        }
        TerminatorKind::SwitchInt { targets, .. } => {
            for (_, target) in &mut targets.cases {
                *target = remap[target.index()];
            }
            targets.otherwise = remap[targets.otherwise.index()];
        }
        TerminatorKind::Call { target, .. } => {
            if let Some(bb) = target {
                *bb = remap[bb.index()];
            }
        }
        TerminatorKind::Drop { target, unwind, .. } => {
            *target = remap[target.index()];
            if let Some(u) = unwind {
                *u = remap[u.index()];
            }
        }
        TerminatorKind::Assert { target, .. } => {
            *target = remap[target.index()];
        }
        TerminatorKind::Return | TerminatorKind::Unreachable => {}
    }
}
