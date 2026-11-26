//! C11 function, statement, expression, and terminator emission.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use ae_mir::{
    AggregateKind, AssertMessage, BinOp, Body, ConstValue, Operand, Place, ProjectionElem, Rvalue,
    StatementKind, TerminatorKind, UnOp,
};
use ae_ty::{DefId, FloatTy, IntTy, TyKind};

use crate::types::{ModuleTypes, c_type_name, int_c_bounds};
use crate::{CodegenError, EmitConfig};

pub(crate) fn build_fn_name_map(bodies: &[Body]) -> (Vec<String>, BTreeMap<DefId, String>) {
    let mut names = Vec::with_capacity(bodies.len());
    let mut used = BTreeMap::<String, usize>::new();
    let mut def_map = BTreeMap::new();

    for body in bodies {
        let base = sanitize_fn_name(&body.name);
        let count = used.entry(base.clone()).or_insert(0);
        let unique = if *count == 0 {
            base
        } else {
            format!("{base}_{}", *count)
        };
        *count += 1;

        if let Some(def_id) = body.def {
            def_map.insert(def_id, unique.clone());
        }
        names.push(unique);
    }

    (names, def_map)
}

fn sanitize_fn_name(name: &str) -> String {
    let mut out = String::from("ae_fn_");
    if name.is_empty() {
        out.push_str("anon");
        return out;
    }
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

pub(crate) fn emit_fn_decl(out: &mut String, body: &Body, c_name: &str) {
    let ret_ty = body
        .locals
        .first()
        .map(|l| l.ty)
        .unwrap_or(TyKind::Unit);
    let ret_c_ty = c_type_name(ret_ty);
    let _ = write!(out, "{ret_c_ty} {c_name}(");
    if body.arg_count == 0 {
        out.push_str("void");
    } else {
        for i in 0..body.arg_count {
            if i > 0 {
                out.push_str(", ");
            }
            let local_idx = i + 1;
            let arg_ty = body
                .locals
                .get(local_idx)
                .map(|l| l.ty)
                .unwrap_or(TyKind::Unit);
            let _ = write!(out, "{} _{local_idx}", c_type_name(arg_ty));
        }
    }
    out.push(')');
}

pub(crate) fn emit_fn_body(
    out: &mut String,
    body: &Body,
    c_name: &str,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<(), CodegenError> {
    emit_fn_decl(out, body, c_name);
    out.push_str(" {\n");

    // Declare return place _0 and non-arg locals (_1..=arg_count are parameters).
    for (idx, decl) in body.locals.iter().enumerate() {
        if idx >= 1 && idx <= body.arg_count {
            let _ = writeln!(out, "    (void)_{idx};");
            continue;
        }
        let c_ty = c_type_name(decl.ty);
        let _ = writeln!(out, "    {c_ty} _{idx} = {{0}};");
        let _ = writeln!(out, "    (void)_{idx};");
    }

    let bb0_targeted = body.basic_blocks.iter().any(|bb| {
        bb.terminator
            .as_ref()
            .is_some_and(|t| t.kind.successors().contains(&ae_mir::BasicBlockId::ENTRY))
    });

    for (bb_idx, bb) in body.basic_blocks.iter().enumerate() {
        if bb_idx == 0 && !bb0_targeted {
            out.push_str("    if (0) { goto bb0; }\n");
        }
        let _ = writeln!(out, "bb{bb_idx}: ;");

        for stmt in &bb.statements {
            emit_statement(out, body, &stmt.kind, types, def_map)?;
        }

        if let Some(term) = &bb.terminator {
            emit_terminator(out, body, &term.kind, types, def_map)?;
        } else {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("bb{bb_idx} missing terminator"),
            });
        }
    }

    out.push_str("}\n\n");
    Ok(())
}

pub(crate) fn emit_main_wrapper(
    out: &mut String,
    bodies: &[Body],
    fn_names: &[String],
    _config: &EmitConfig,
) -> Result<(), CodegenError> {
    if bodies.is_empty() {
        return Err(CodegenError::InvalidEntry {
            detail: "cannot emit main wrapper for an empty module".to_owned(),
        });
    }

    let entry_idx = bodies
        .iter()
        .position(|b| b.name == "main")
        .unwrap_or(0);
    let entry_body = &bodies[entry_idx];
    let entry_c_name = &fn_names[entry_idx];

    if entry_body.arg_count != 0 {
        return Err(CodegenError::InvalidEntry {
            detail: format!(
                "entry function `{}` must take 0 arguments for main wrapper (found {})",
                entry_body.name, entry_body.arg_count
            ),
        });
    }

    let ret_ty = entry_body
        .locals
        .first()
        .map(|l| l.ty)
        .unwrap_or(TyKind::Unit);

    out.push_str("int main(void) {\n");
    match ret_ty {
        TyKind::Int(_) | TyKind::Char => {
            let _ = writeln!(out, "    return (int){entry_c_name}();");
        }
        TyKind::Bool => {
            let _ = writeln!(out, "    return {entry_c_name}() ? 1 : 0;");
        }
        _ => {
            let _ = writeln!(out, "    (void){entry_c_name}();");
            out.push_str("    return 0;\n");
        }
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_statement(
    out: &mut String,
    body: &Body,
    kind: &StatementKind,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<(), CodegenError> {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            let rhs_ty = types.infer_rvalue_ty(body, rvalue);
            let lhs_ty = types.infer_place_ty(body, place);
            let pointee_ty = rhs_ty.or(lhs_ty);
            let place_expr = emit_place(body, place, types, pointee_ty)?;
            let rhs_expr = emit_rvalue(body, rvalue, lhs_ty, types, def_map)?;
            let _ = writeln!(out, "    {place_expr} = {rhs_expr};");
        }
        StatementKind::StorageLive(local) => {
            let _ = writeln!(out, "    /* storage live _{} */", local.0);
        }
        StatementKind::StorageDead(local) => {
            let _ = writeln!(out, "    /* storage dead _{} */", local.0);
        }
        StatementKind::SetDiscriminant {
            place,
            variant_index,
        } => {
            let place_expr = emit_place(body, place, types, None)?;
            let _ = writeln!(out, "    {place_expr}.tag = {variant_index};");
        }
        StatementKind::Drop(place) => {
            let place_expr = emit_place(body, place, types, None)?;
            let _ = writeln!(out, "    /* drop {place_expr} */");
        }
        StatementKind::Nop => {
            out.push_str("    /* nop */\n");
        }
    }
    Ok(())
}

fn emit_terminator(
    out: &mut String,
    body: &Body,
    kind: &TerminatorKind,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<(), CodegenError> {
    match kind {
        TerminatorKind::Goto { target } => {
            let _ = writeln!(out, "    goto bb{};", target.0);
        }
        TerminatorKind::SwitchInt { discr, targets } => {
            let discr_expr = emit_operand(body, discr, types, def_map)?;
            let _ = writeln!(out, "    switch ((uint64_t)({discr_expr})) {{");
            for (val, target) in &targets.cases {
                let val_u64 = u64::try_from(*val).map_err(|_| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("SwitchInt case value {val} exceeds 64 bits"),
                })?;
                let _ = writeln!(out, "        case UINT64_C({val_u64}): goto bb{};", target.0);
            }
            let _ = writeln!(out, "        default: goto bb{};", targets.otherwise.0);
            out.push_str("    }\n");
        }
        TerminatorKind::Call {
            func,
            args,
            destination,
            target,
        } => {
            let callee = match func {
                Operand::Constant(c) => match &c.value {
                    ConstValue::FnDef(def_id) => {
                        def_map.get(def_id).cloned().ok_or_else(|| {
                            CodegenError::UnresolvedCall {
                                body_name: body.name.clone(),
                                def_id: *def_id,
                            }
                        })?
                    }
                    other => {
                        return Err(CodegenError::UnsupportedFeature {
                            body_name: body.name.clone(),
                            detail: format!("cannot call non-function constant {other:?}"),
                        });
                    }
                },
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("indirect call operand {other:?} is not supported"),
                    });
                }
            };

            let mut arg_exprs = Vec::with_capacity(args.len());
            for arg in args {
                arg_exprs.push(emit_operand(body, arg, types, def_map)?);
            }
            let dest_ty = types.infer_place_ty(body, destination);
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {dest_expr} = {callee}({});",
                arg_exprs.join(", ")
            );
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
        }
        TerminatorKind::Drop { target, .. } => {
            let _ = writeln!(out, "    goto bb{};", target.0);
        }
        TerminatorKind::Assert {
            cond,
            expected,
            msg,
            target,
        } => {
            let cond_expr = emit_operand(body, cond, types, def_map)?;
            let expected_str = if *expected { "true" } else { "false" };
            let msg_str = escape_c_string(&format_assert_message(msg));
            let _ = writeln!(
                out,
                "    if (({cond_expr}) != {expected_str}) {{ ae_trap(\"{msg_str}\"); }}"
            );
            let _ = writeln!(out, "    goto bb{};", target.0);
        }
        TerminatorKind::Return => {
            out.push_str("    return _0;\n");
        }
        TerminatorKind::Unreachable => {
            out.push_str("    ae_trap(\"reached unreachable\");\n");
        }
    }
    Ok(())
}

fn format_assert_message(msg: &AssertMessage) -> String {
    match msg {
        AssertMessage::BoundsCheck { .. } => "index out of bounds".to_owned(),
        AssertMessage::DivisionByZero(_) => "attempt to divide by zero".to_owned(),
        AssertMessage::RemainderByZero(_) => "attempt to calculate remainder with zero".to_owned(),
        AssertMessage::Overflow(op, _, _) => format!("attempt to compute {op:?} with overflow"),
        AssertMessage::Custom(s) => s.clone(),
    }
}

fn emit_place(
    body: &Body,
    place: &Place,
    types: &ModuleTypes,
    deref_pointee_ty: Option<TyKind>,
) -> Result<String, CodegenError> {
    let base_decl = body
        .local_decl(place.local)
        .ok_or_else(|| CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: format!("local _{} out of bounds", place.local.0),
        })?;

    let mut expr = format!("_{}", place.local.0);
    let mut cur_ty = Some(base_decl.ty);
    let mut active_variant: Option<usize> = None;

    for (idx, elem) in place.projection.iter().enumerate() {
        let is_last = idx + 1 == place.projection.len();
        match elem {
            ProjectionElem::Downcast(v) => {
                active_variant = Some(*v);
            }
            ProjectionElem::Field(fidx) => match cur_ty {
                Some(TyKind::Struct(id)) => {
                    expr = format!("({expr}.f{fidx})");
                    cur_ty = types
                        .structs
                        .get(&id)
                        .and_then(|fs| fs.get(*fidx))
                        .and_then(|t| *t);
                }
                Some(TyKind::Closure(id)) => {
                    expr = format!("({expr}.f{fidx})");
                    cur_ty = types
                        .closures
                        .get(&id)
                        .and_then(|fs| fs.get(*fidx))
                        .and_then(|t| *t);
                }
                Some(TyKind::Enum(_, id)) => {
                    let v = active_variant.unwrap_or(0);
                    expr = format!("({expr}.u.v{v}.f{fidx})");
                    cur_ty = types
                        .enums
                        .get(&id)
                        .and_then(|vs| vs.get(&v))
                        .and_then(|fs| fs.get(*fidx))
                        .and_then(|t| *t);
                }
                other => {
                    return Err(CodegenError::UnsupportedType {
                        body_name: body.name.clone(),
                        detail: format!("cannot project Field({fidx}) on {other:?}"),
                    });
                }
            },
            ProjectionElem::Index(idx_local) => match cur_ty {
                Some(TyKind::ArrayOf(elem_id, len)) => {
                    expr = format!("({expr}.data[(size_t)_{}])", idx_local.0);
                    cur_ty = types.arrays.get(&(elem_id, len)).and_then(|t| *t);
                }
                other => {
                    return Err(CodegenError::UnsupportedType {
                        body_name: body.name.clone(),
                        detail: format!("cannot index into {other:?}"),
                    });
                }
            },
            ProjectionElem::Deref => {
                let target_ty = if is_last {
                    deref_pointee_ty
                } else {
                    None
                };
                let pointee = target_ty.ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "cannot dereference pointer without known pointee type".to_owned(),
                })?;
                let c_ty = c_type_name(pointee);
                expr = format!("(*(({c_ty} *)({expr})))");
                cur_ty = Some(pointee);
            }
        }
    }

    Ok(expr)
}

fn emit_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            let ty = types.infer_place_ty(body, place);
            emit_place(body, place, types, ty)
        }
        Operand::Constant(c) => emit_constant(body, &c.value, c.ty, def_map),
    }
}

fn emit_constant(
    body: &Body,
    value: &ConstValue,
    ty: TyKind,
    def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    match (value, ty) {
        (ConstValue::Unit, TyKind::Unit) => Ok("((ae_unit_t){ 0 })".to_owned()),
        (ConstValue::Bool(b), TyKind::Bool) => Ok(if *b { "true" } else { "false" }.to_owned()),
        (ConstValue::Char(ch), TyKind::Char) => Ok(format!("UINT32_C(0x{:x})", *ch as u32)),
        (ConstValue::Int(v), TyKind::Int(ity)) => emit_int_constant(*v, ity, &body.name),
        (ConstValue::Float(f), TyKind::Float(fty)) => Ok(emit_float_constant(f.to_f64(), fty)),
        (ConstValue::String(s), TyKind::String | TyKind::StrView) => {
            Ok(format!("\"{}\"", escape_c_string(s)))
        }
        (ConstValue::FnDef(def_id), TyKind::Fn(_) | TyKind::Closure(_)) => {
            let name = def_map.get(def_id).ok_or_else(|| CodegenError::UnresolvedCall {
                body_name: body.name.clone(),
                def_id: *def_id,
            })?;
            Ok(format!("((void *){name})"))
        }
        _ => Err(CodegenError::UnsupportedType {
            body_name: body.name.clone(),
            detail: format!("constant {value:?} incompatible with type {ty:?}"),
        }),
    }
}

fn emit_int_constant(v: i128, ity: IntTy, body_name: &str) -> Result<String, CodegenError> {
    if !ity.contains(v) {
        return Err(CodegenError::UnsupportedFeature {
            body_name: body_name.to_owned(),
            detail: format!("integer constant {v} out of range for {ity:?}"),
        });
    }
    let c_ty = ity.c_name();
    if ity.signed() {
        if v == ity.min_value() {
            let (min_expr, _) = int_c_bounds(ity);
            Ok(min_expr.to_owned())
        } else {
            Ok(format!("(({c_ty}){v}LL)"))
        }
    } else {
        Ok(format!("(({c_ty}){v}ULL)"))
    }
}

fn emit_float_constant(val: f64, fty: FloatTy) -> String {
    let suffix = match fty {
        FloatTy::F32 => "f",
        FloatTy::F64 => "",
    };
    if val.is_nan() {
        return "((double)NAN)".to_owned();
    }
    if val.is_infinite() {
        return if val.is_sign_negative() {
            format!("(-({}INFINITY))", if matches!(fty, FloatTy::F32) { "(float)" } else { "(double)" })
        } else {
            format!("({}INFINITY)", if matches!(fty, FloatTy::F32) { "(float)" } else { "(double)" })
        };
    }
    let mut s = format!("{val:?}");
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    format!("{s}{suffix}")
}

fn emit_rvalue(
    body: &Body,
    rvalue: &Rvalue,
    expected_ty: Option<TyKind>,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    match rvalue {
        Rvalue::Use(op) => emit_operand(body, op, types, def_map),
        Rvalue::Ref { place, .. } => {
            let place_expr = emit_place(body, place, types, None)?;
            Ok(format!("((void *)&({place_expr}))"))
        }
        Rvalue::UnaryOp(op, operand) => {
            let expr = emit_operand(body, operand, types, def_map)?;
            let ty = types.infer_operand_ty(body, operand);
            match (op, ty) {
                (UnOp::Not, Some(TyKind::Bool)) => Ok(format!("(!({expr}))")),
                (UnOp::Not, Some(TyKind::Int(ity))) => {
                    let c_ty = ity.c_name();
                    Ok(format!("(({c_ty})(~({expr})))"))
                }
                (UnOp::Neg, Some(TyKind::Int(ity))) if ity.signed() => {
                    Ok(format!("ae_neg_{}({expr})", ity.name()))
                }
                (UnOp::Neg, Some(TyKind::Float(_))) => Ok(format!("(-({expr}))")),
                _ => Err(CodegenError::UnsupportedType {
                    body_name: body.name.clone(),
                    detail: format!("unsupported unary op {op:?} on {ty:?}"),
                }),
            }
        }
        Rvalue::BinaryOp(op, lhs, rhs) => {
            let l_expr = emit_operand(body, lhs, types, def_map)?;
            let r_expr = emit_operand(body, rhs, types, def_map)?;
            let ty = types.infer_operand_ty(body, lhs);
            emit_binary(body, *op, ty, &l_expr, &r_expr)
        }
        Rvalue::Cast(kind, operand, target_ty) => {
            let expr = emit_operand(body, operand, types, def_map)?;
            let src_ty = types.infer_operand_ty(body, operand);
            let target_c_ty = c_type_name(*target_ty);
            if matches!(kind, ae_mir::CastKind::Misc) && matches!(src_ty, Some(TyKind::Bool)) {
                Ok(format!("(({target_c_ty})(({expr}) ? 1 : 0))"))
            } else {
                Ok(format!("(({target_c_ty})({expr}))"))
            }
        }
        Rvalue::Aggregate(kind, ops) => {
            let mut op_exprs = Vec::with_capacity(ops.len());
            for op in ops {
                op_exprs.push(emit_operand(body, op, types, def_map)?);
            }
            match kind {
                AggregateKind::Struct(id) => {
                    if op_exprs.is_empty() {
                        Ok(format!("((ae_struct_{id}){{ 0 }})"))
                    } else {
                        let fields: Vec<String> = op_exprs
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!(".f{i} = {e}"))
                            .collect();
                        Ok(format!("((ae_struct_{id}){{ {} }})", fields.join(", ")))
                    }
                }
                AggregateKind::Closure(id) => {
                    if op_exprs.is_empty() {
                        Ok(format!("((ae_closure_{id}){{ 0 }})"))
                    } else {
                        let fields: Vec<String> = op_exprs
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!(".f{i} = {e}"))
                            .collect();
                        Ok(format!("((ae_closure_{id}){{ {} }})", fields.join(", ")))
                    }
                }
                AggregateKind::Enum(id, variant) => {
                    if op_exprs.is_empty() {
                        Ok(format!("((ae_enum_{id}){{ .tag = {variant} }})"))
                    } else {
                        let fields: Vec<String> = op_exprs
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!(".f{i} = {e}"))
                            .collect();
                        Ok(format!(
                            "((ae_enum_{id}){{ .tag = {variant}, .u = {{ .v{variant} = {{ {} }} }} }})",
                            fields.join(", ")
                        ))
                    }
                }
                AggregateKind::Array(_) => {
                    let arr_ty = expected_ty
                        .or_else(|| types.infer_rvalue_ty(body, rvalue))
                        .ok_or_else(|| CodegenError::UnsupportedType {
                            body_name: body.name.clone(),
                            detail: "cannot determine ArrayOf type for AggregateKind::Array"
                                .to_owned(),
                        })?;
                    let c_ty = c_type_name(arr_ty);
                    if op_exprs.is_empty() {
                        Ok(format!("(({c_ty}){{ 0 }})"))
                    } else {
                        Ok(format!("(({c_ty}){{ .data = {{ {} }} }})", op_exprs.join(", ")))
                    }
                }
                AggregateKind::Tuple => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "AggregateKind::Tuple is not supported without a tuple TyKind"
                        .to_owned(),
                }),
            }
        }
        Rvalue::Len(place) => {
            let ty = types.infer_place_ty(body, place);
            match ty {
                Some(TyKind::ArrayOf(_, len)) => Ok(format!("((uintptr_t){len}ULL)")),
                Some(TyKind::String | TyKind::StrView) => {
                    let place_expr = emit_place(body, place, types, None)?;
                    Ok(format!("((uintptr_t)strlen({place_expr}))"))
                }
                _ => Err(CodegenError::UnsupportedType {
                    body_name: body.name.clone(),
                    detail: format!("Rvalue::Len requires array or string, found {ty:?}"),
                }),
            }
        }
        Rvalue::Discriminant(place) => {
            let place_expr = emit_place(body, place, types, None)?;
            Ok(format!("((int64_t)({place_expr}.tag))"))
        }
    }
}

fn emit_binary(
    body: &Body,
    op: BinOp,
    ty: Option<TyKind>,
    l: &str,
    r: &str,
) -> Result<String, CodegenError> {
    if op.is_comparison() {
        let cmp = match op {
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            _ => unreachable!(),
        };
        return Ok(format!("(({l}) {cmp} ({r}))"));
    }

    match ty {
        Some(TyKind::Int(ity)) => {
            let name = ity.name();
            let c_ty = ity.c_name();
            match op {
                BinOp::Add => Ok(format!("ae_add_{name}(({l}), ({r}))")),
                BinOp::Sub => Ok(format!("ae_sub_{name}(({l}), ({r}))")),
                BinOp::Mul => Ok(format!("ae_mul_{name}(({l}), ({r}))")),
                BinOp::Div => Ok(format!("ae_div_{name}(({l}), ({r}))")),
                BinOp::Rem => Ok(format!("ae_rem_{name}(({l}), ({r}))")),
                BinOp::Shl => Ok(format!("ae_shl_{name}(({l}), ({r}))")),
                BinOp::Shr => Ok(format!("ae_shr_{name}(({l}), ({r}))")),
                BinOp::BitAnd => Ok(format!("(({c_ty})(({l}) & ({r})))")),
                BinOp::BitOr => Ok(format!("(({c_ty})(({l}) | ({r})))")),
                BinOp::BitXor => Ok(format!("(({c_ty})(({l}) ^ ({r})))")),
                _ => unreachable!(),
            }
        }
        Some(TyKind::Bool) => match op {
            BinOp::BitAnd => Ok(format!("((bool)(({l}) & ({r})))")),
            BinOp::BitOr => Ok(format!("((bool)(({l}) | ({r})))")),
            BinOp::BitXor => Ok(format!("((bool)(({l}) ^ ({r})))")),
            _ => Err(CodegenError::UnsupportedType {
                body_name: body.name.clone(),
                detail: format!("unsupported binary op {op:?} on Bool"),
            }),
        },
        Some(TyKind::Float(fty)) => match op {
            BinOp::Add => Ok(format!("(({l}) + ({r}))")),
            BinOp::Sub => Ok(format!("(({l}) - ({r}))")),
            BinOp::Mul => Ok(format!("(({l}) * ({r}))")),
            BinOp::Div => Ok(format!("(({l}) / ({r}))")),
            BinOp::Rem => match fty {
                FloatTy::F32 => Ok(format!("fmodf(({l}), ({r}))")),
                FloatTy::F64 => Ok(format!("fmod(({l}), ({r}))")),
            },
            _ => Err(CodegenError::UnsupportedType {
                body_name: body.name.clone(),
                detail: format!("unsupported binary op {op:?} on Float"),
            }),
        },
        other => Err(CodegenError::UnsupportedType {
            body_name: body.name.clone(),
            detail: format!("unsupported binary op {op:?} on {other:?}"),
        }),
    }
}

fn escape_c_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            other => {
                let _ = write!(out, "\\x{other:02x}");
            }
        }
    }
    out
}
