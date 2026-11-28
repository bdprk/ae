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
        // A `#[no_mangle]` exposure keeps its own name verbatim: the symbol
        // is part of the unit's C interface, with external linkage (M2.12 U7).
        let base = match &body.exposure {
            Some(c_name) => c_name.clone(),
            None => sanitize_fn_name(&body.name),
        };
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
            emit_statement(out, body, stmt, types, def_map)?;
        }

        if let Some(term) = &bb.terminator {
            emit_terminator(out, body, term, types, def_map)?;
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

    out.push_str(
        "int main(int argc, char **argv) {\n    ae_argc_g = argc;\n    ae_argv_g = argv;\n",
    );
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
    stmt: &ae_mir::Statement,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<(), CodegenError> {
    match &stmt.kind {
        StatementKind::Assign(place, rvalue) => {
            let rhs_ty = types.infer_rvalue_ty(body, rvalue);
            let lhs_ty = types.infer_place_ty(body, place);
            let pointee_ty = rhs_ty.or(lhs_ty);
            let place_expr = emit_place(body, place, types, pointee_ty)?;
            let rhs_expr = emit_rvalue(body, rvalue, lhs_ty, types, def_map, stmt.span.clone())?;
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
            emit_drop(out, body, place, types)?;
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
    term: &ae_mir::Terminator,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<(), CodegenError> {
    match &term.kind {
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
            let intrinsic = match func {
                Operand::Constant(c) => match &c.value {
                    ConstValue::FnDef(def_id) => body.intrinsics.get(def_id).cloned(),
                    _ => None,
                },
                _ => None,
            };
            if let Some(name) = intrinsic {
                // A builtin intrinsic: the backend emits it itself.
                let mut arg_exprs = Vec::with_capacity(args.len());
                for arg in args {
                    arg_exprs.push(emit_operand(body, arg, types, def_map)?);
                }
                emit_intrinsic(
                    IntrinsicCall {
                        body,
                        name: &name,
                        args,
                        arg_exprs: &arg_exprs,
                        destination,
                        target: *target,
                        def_map,
                        span: term.span.clone(),
                    },
                    types,
                    out,
                )?;
                return Ok(());
            }
            let callee = match func {
                Operand::Constant(c) => match &c.value {
                    ConstValue::FnDef(def_id) => {
                        if let Some(symbol) = body.externs.get(def_id) {
                            // A symbol this unit defines itself (a `#[no_mangle]`
                            // exposure) is a local call: the foreign declaration
                            // resolves to the definition later in the file, exactly
                            // like a real C translation unit.
                            if !def_map.values().any(|name| name == symbol)
                                && !runtime_extern(symbol)
                            {
                                return Err(CodegenError::UnsupportedFeature {
                                    body_name: body.name.clone(),
                                    detail: format!(
                                        "extern '{symbol}' is not linked by the pure C backend yet"
                                    ),
                                });
                            }
                            symbol.clone()
                        } else {
                            def_map.get(def_id).cloned().ok_or_else(|| {
                                CodegenError::UnresolvedCall {
                                    body_name: body.name.clone(),
                                    def_id: *def_id,
                                }
                            })?
                        }
                    }
                    other => {
                        return Err(CodegenError::UnsupportedFeature {
                            body_name: body.name.clone(),
                            detail: format!("cannot call non-function constant {other:?}"),
                        });
                    }
                },
                // A function-typed binding holds the pointer: call the value
                // through a parenthesized expression (its C type is the
                // signature typedef, so no cast is needed).
                operand => format!("({})", emit_operand(body, operand, types, def_map)?),
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
        TerminatorKind::Drop { place, target, .. } => {
            emit_drop(out, body, place, types)?;
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
            // A span on the terminator names the trap site (ADR-0240); without
            // one the trap reports the historical `ae:0:0`.
            match &term.span {
                Some(span) => {
                    let _ = writeln!(
                        out,
                        "    if (({cond_expr}) != {expected_str}) {{ ae_fail_at(\"{msg_str}\", (size_t){}, (size_t){}); }}",
                        span.line, span.column
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "    if (({cond_expr}) != {expected_str}) {{ ae_trap(\"{msg_str}\"); }}"
                    );
                }
            }
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

/// Emit a builtin intrinsic call (`print`, `exit`) that the pure C backend
/// provides itself. Unknown intrinsics fail closed with a named error, so an
/// unsupported builtin never turns into a link error.
struct IntrinsicCall<'a> {
    body: &'a Body,
    name: &'a str,
    args: &'a [Operand],
    arg_exprs: &'a [String],
    destination: &'a Place,
    target: Option<ae_mir::BasicBlockId>,
    def_map: &'a BTreeMap<DefId, String>,
    span: Option<ae_span::Span>,
}

fn emit_intrinsic(
    call: IntrinsicCall<'_>,
    types: &ModuleTypes,
    out: &mut String,
) -> Result<(), CodegenError> {
    let IntrinsicCall { body, name, args, arg_exprs, destination, target, def_map, span } = call;
    let (line, column) = span.map(|s| (s.line, s.column)).unwrap_or((0, 0));
    match name {
        "print" => {
            let arg = args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: "print needs one value".to_owned(),
            })?;
            let expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let (printer, rendered) = match types.infer_operand_ty(body, arg) {
                Some(TyKind::Int(width)) if width.signed() => {
                    ("ae_print_i64", format!("(int64_t)({expr})"))
                }
                Some(TyKind::Int(_)) => ("ae_print_u64", format!("(uint64_t)({expr})")),
                Some(TyKind::Bool) => ("ae_print_bool", expr.to_owned()),
                Some(TyKind::Char) => ("ae_print_char", expr.to_owned()),
                Some(TyKind::Float(FloatTy::F32)) => ("ae_print_f64", format!("(double)({expr})")),
                Some(TyKind::Float(FloatTy::F64)) => ("ae_print_f64", expr.to_owned()),
                Some(TyKind::String | TyKind::StrView) => ("ae_print_str", expr.to_owned()),
                Some(other) => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("print of {other:?} is not supported by the pure C backend yet"),
                    });
                }
                None => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: "print operand has no inferable type".to_owned(),
                    });
                }
            };
            let _ = writeln!(out, "    {printer}({rendered});");
            let dest_ty = types.infer_place_ty(body, destination);
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(out, "    {dest_expr} = (ae_unit_t){{ 0 }};");
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "exit" => {
            let expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let _ = writeln!(out, "    ae_exit((int)({expr}));");
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            }
            Ok(())
        }
        "str_concat" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "str_concat needs two string operands".to_owned(),
                });
            }
            let a = emit_owned_operand(body, &args[0], types, def_map)?;
            let b = emit_owned_operand(body, &args[1], types, def_map)?;
            // The result allocation names the concat's position, so an
            // exhausted AE_ALLOC_LIMIT reports the expression, not ae:0:0.
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_str_concat_at({a}, {b}, (size_t){line}, (size_t){column})"),
            )
        }
        "str_eq" | "str_ne" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs two string operands"),
                });
            }
            let a = emit_operand(body, &args[0], types, def_map)?;
            let b = emit_operand(body, &args[1], types, def_map)?;
            let call = format!("ae_str_eq({a}, {b})");
            let call = if name == "str_ne" { format!("(!{call})") } else { call };
            finish_value(out, body, types, destination, target, &call)
        }
        "string_view" => {
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_str_view({value})"),
            )
        }
        "slice" | "slice_view" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a base, a start and an end"),
                });
            }
            let base = emit_operand(body, &args[0], types, def_map)?;
            let start = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let end = arg_exprs.get(2).map(String::as_str).unwrap_or("0");
            let helper = if name == "slice" { "ae_str_slice" } else { "ae_str_slice_view" };
            let call = format!("{helper}({base}, (int64_t)({start}), (int64_t)({end}), (size_t){line}, (size_t){column})");
            finish_value(out, body, types, destination, target, &call)
        }
        "to_string" => {
            let arg = args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: "to_string needs one value".to_owned(),
            })?;
            let expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let call = match types.infer_operand_ty(body, arg) {
                Some(TyKind::Int(IntTy::U64 | IntTy::Usize)) => {
                    format!("ae_str_from_u64((uint64_t)({expr}))")
                }
                Some(TyKind::Int(_)) => format!("ae_str_from_i64((int64_t)({expr}))"),
                Some(TyKind::Char) => format!("ae_str_from_char({expr})"),
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("to_string of {other:?} is not supported"),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &call)
        }
        "float_to_string" => {
            let arg = args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: "float_to_string needs one value".to_owned(),
            })?;
            let expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let call = match types.infer_operand_ty(body, arg) {
                Some(TyKind::Float(FloatTy::F32)) => {
                    format!("ae_str_from_f64((double)({expr}))")
                }
                Some(TyKind::Float(FloatTy::F64)) => format!("ae_str_from_f64({expr})"),
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("float_to_string of {other:?} is not supported"),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &call)
        }
        "vec_new" | "strvec_new" | "f64vec_new" => {
            // A vector literal (or an empty constructor) builds its header
            // in the destination and pushes every operand in source order.
            let dest_ty = types.infer_place_ty(body, destination);
            let id = match dest_ty {
                Some(TyKind::Vector(id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("cannot build a vector into {other:?}"),
                    });
                }
            };
            let elem = types.vector_elem(id).ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("vector element type for id {id} is not carried on the body"),
            })?;
            if !types.elem_supported(elem) {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("vector elements of {elem:?} are not supported by the pure C backend yet"),
                });
            }
            let c_elem = c_type_name(elem);
            let drop = types.elem_drop_expr(elem);
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(out, "    {dest_expr} = ae_vec_new((int64_t)sizeof({c_elem}), {drop});");
            for op in args {
                let value = emit_owned_operand(body, op, types, def_map)?;
                // A block-scoped temp carries the value: a compound literal
                // `(T){ value }` would initialize the first member instead of
                // copying a whole struct value.
                let _ = writeln!(
                    out,
                    "    {{ {c_elem} _tmp = {value}; ae_vec_push(&{dest_expr}, &_tmp); }}"
                );
            }
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "vec_push" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_push needs a vector and a value".to_owned(),
                });
            }
            let (ptr, elem) = vector_operand(body, &args[0], types, def_map)?;
            let c_elem = c_type_name(elem);
            let value = emit_owned_operand(body, &args[1], types, def_map)?;
            let _ = writeln!(
                out,
                "    {{ {c_elem} _tmp = {value}; ae_vec_push({ptr}, &_tmp); }}"
            );
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "vec_pop" => {
            let (ptr, _elem) = vector_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_pop needs a vector".to_owned(),
                })?,
                types,
                def_map,
            )?;
            let _ = writeln!(out, "    ae_vec_pop({ptr}, (size_t){line}, (size_t){column});");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "vec_check" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_check needs a vector and an index".to_owned(),
                });
            }
            let (ptr, _elem) = vector_operand(body, &args[0], types, def_map)?;
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let call = format!(
                "ae_vec_check({ptr}, (int64_t)({index}), (size_t){line}, (size_t){column})"
            );
            finish_value(out, body, types, destination, target, &call)
        }
        "vec_store" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_store needs a vector, an index and a value".to_owned(),
                });
            }
            let (ptr, elem) = vector_operand(body, &args[0], types, def_map)?;
            let c_elem = c_type_name(elem);
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let value = emit_owned_operand(body, &args[2], types, def_map)?;
            // The index is resolved (and bounds-checked) through ae_vec_at
            // before the value is materialized: a write whose index is out of
            // bounds must not first evaluate a trapping value expression.
            let _ = writeln!(
                out,
                "    {{ void *_slot = ae_vec_at({ptr}, (int64_t)({index}), (size_t){line}, (size_t){column});\n      {c_elem} _tmp = {value};\n      if (({ptr})->drop) ({ptr})->drop(_slot);\n      memcpy(_slot, &_tmp, (size_t)({ptr})->elem_size); }}"
            );
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "vec_index" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_index needs a vector and an index".to_owned(),
                });
            }
            let (ptr, elem) = vector_operand(body, &args[0], types, def_map)?;
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let access = format!("ae_vec_at({ptr}, (int64_t)({index}), (size_t){line}, (size_t){column})");
            let dest_ty = types.infer_place_ty(body, destination);
            let value = match (dest_ty, elem) {
                // An owned element read is a view: the vector keeps the
                // storage and its release stays the only one.
                (Some(TyKind::String | TyKind::StrView), TyKind::String) => {
                    format!("ae_str_view(*(ae_str *){access})")
                }
                (Some(TyKind::StructRef(_) | TyKind::StructMut(_)), _) => {
                    format!("((void *){access})")
                }
                (Some(TyKind::VectorRef(_)), _) => format!("((ae_vec *){access})"),
                // A by-value read of a nested vector is a header copy used
                // by a read-only consumer (len/slice); the outer vector keeps
                // the storage and its release stays the only one.
                (Some(TyKind::Vector(_)), TyKind::Vector(_)) => {
                    format!("(*((ae_vec *){access}))")
                }
                // A by-value read of an element (a scalar, or a temporary aggregate used
// for a field chain) is a copy; the frontend rejects binding an owned
// aggregate element, so this copy never becomes a second owner.
                (Some(dest), _) if dest == elem => {
                    format!("(*(({} *){access}))", c_type_name(dest))
                }
                (other, _) => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!(
                            "vector element read as {other:?} from {elem:?} elements is not supported yet"
                        ),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &value)
        }
        "vec_index_of" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "vec_index_of needs a vector and an index".to_owned(),
                });
            }
            let (ptr, elem) = vector_operand(body, &args[0], types, def_map)?;
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let access = format!("ae_vec_at({ptr}, (int64_t)({index}), (size_t){line}, (size_t){column})");
            let dest_ty = types.infer_place_ty(body, destination);
            let value = match dest_ty {
                Some(TyKind::VectorRef(_)) => format!("((ae_vec *){access})"),
                Some(TyKind::StructRef(_) | TyKind::StructMut(_)) => format!("((void *){access})"),
                Some(TyKind::Ptr(_) | TyKind::PtrMut(_)) => format!("((void *){access})"),
                Some(TyKind::StrView) if elem == TyKind::String => {
                    format!("ae_str_view(*(ae_str *){access})")
                }
                // `vec_index_of` also types a field-receiver read
                // (`w.cells[i]`) as the element itself; that temporary is
                // read (or field-projected) and never released.
                Some(TyKind::Vector(_)) => format!("(*((ae_vec *){access}))"),
                Some(dest) if dest == elem => {
                    format!("(*(({} *){access}))", c_type_name(dest))
                }
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("borrowed vector element as {other:?} is not supported yet"),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &value)
        }
        "slice_of" => {
            let arg = args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: "slice_of needs a source".to_owned(),
            })?;
            let place = match arg {
                Operand::Copy(place) | Operand::Move(place) => place,
                Operand::Constant(c) => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("slice_of cannot view a constant {:?}", c.value),
                    });
                }
            };
            let (view, _elem) = slice_view_of_place(body, place, types)?;
            finish_value(out, body, types, destination, target, &view)
        }
        "slice_index" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "slice_index needs a slice and an index".to_owned(),
                });
            }
            let (slice_expr, elem) = slice_operand(body, &args[0], types, def_map)?;
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let access = format!("ae_slice_at({slice_expr}, (int64_t)({index}), (size_t){line}, (size_t){column})");
            let dest_ty = types.infer_place_ty(body, destination);
            let value = match (dest_ty, elem) {
                (Some(TyKind::String | TyKind::StrView), TyKind::String) => {
                    format!("ae_str_view(*(ae_str *){access})")
                }
                (Some(TyKind::StructRef(_) | TyKind::StructMut(_)), _) => {
                    format!("((void *){access})")
                }
                (Some(TyKind::VectorRef(_)), _) => format!("((ae_vec *){access})"),
                (Some(TyKind::Vector(_)), TyKind::Vector(_)) => {
                    format!("(*((ae_vec *){access}))")
                }
                (Some(dest), _) if dest == elem => {
                    format!("(*(({} *){access}))", c_type_name(dest))
                }
                (other, _) => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!(
                            "slice element read as {other:?} from {elem:?} elements is not supported yet"
                        ),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &value)
        }
        "slice_range" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "slice_range needs a slice, a start and an end".to_owned(),
                });
            }
            let (slice_expr, _elem) = slice_operand(body, &args[0], types, def_map)?;
            let start = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let end = arg_exprs.get(2).map(String::as_str).unwrap_or("0");
            let call = format!("ae_slice_range({slice_expr}, (int64_t)({start}), (int64_t)({end}), (size_t){line}, (size_t){column})");
            finish_value(out, body, types, destination, target, &call)
        }
        "slice_store" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "slice_store needs a slice, an index and a value".to_owned(),
                });
            }
            let (slice_expr, elem) = slice_operand(body, &args[0], types, def_map)?;
            let c_elem = c_type_name(elem);
            let index = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let value = emit_owned_operand(body, &args[2], types, def_map)?;
            let _ = writeln!(
                out,
                "    {{ {c_elem} _tmp = {value}; *(({c_elem} *)ae_slice_at_mut(&{slice_expr}, (int64_t)({index}), (size_t){line}, (size_t){column})) = _tmp; }}"
            );
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "bytes" => {
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            match types.infer_place_ty(body, destination) {
                Some(TyKind::Vector(id))
                    if types.vector_elem(id) == Some(TyKind::Int(IntTy::I64)) =>
                {
                    finish_value(out, body, types, destination, target, &format!("ae_vec_bytes({value})"))
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("bytes() into {other:?} is not supported yet"),
                }),
            }
        }
        "sort" | "reverse" | "sort_field" | "reverse_field" | "sort_strings" | "reverse_strings" => {
            let (ptr, elem) = vector_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a vector"),
                })?,
                types,
                def_map,
            )?;
            let call = if name.starts_with("reverse") {
                format!("ae_vec_reverse({ptr})")
            } else {
                match elem {
                    TyKind::Int(IntTy::I64) => format!("ae_vec_sort_i64({ptr})"),
                    TyKind::Float(FloatTy::F64) => format!("ae_vec_sort_f64({ptr})"),
                    TyKind::String => format!("ae_vec_sort_str({ptr})"),
                    other => {
                        return Err(CodegenError::UnsupportedFeature {
                            body_name: body.name.clone(),
                            detail: format!("sorting {other:?} elements is not supported yet"),
                        });
                    }
                }
            };
            let _ = writeln!(out, "    {call};");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "join" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "join needs a string vector and a separator".to_owned(),
                });
            }
            let (ptr, elem) = vector_operand(body, &args[0], types, def_map)?;
            if elem != TyKind::String {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("join of {elem:?} elements is not supported yet"),
                });
            }
            let sep = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_vec_join({ptr}, {sep})"))
        }
        "starts_with" | "ends_with" | "contains" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs two strings"),
                });
            }
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let helper = match name {
                "starts_with" => "ae_str_starts_with",
                "ends_with" => "ae_str_ends_with",
                _ => "ae_str_contains",
            };
            finish_value(out, body, types, destination, target, &format!("{helper}({a}, {b})"))
        }
        "find" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "find needs two strings".to_owned(),
                });
            }
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            // `find` returns IntResult: Ok(index) or Err("substring not
            // found"), like the main runtime.
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("find result is {other:?}, not an IntResult enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::Int(IntTy::I64)])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("find result enum {eid} has no i64 Ok variant"),
                })?;
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("find result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = _at }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = ((ae_str){{ .ptr = \"substring not found\", .len = (int64_t)(sizeof(\"substring not found\") - 1), .owned = false }}) }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ int64_t _at = ae_str_find({a}, {b}); if (_at >= 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "to_f64_checked" => {
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let call = format!(
                "ae_f64_from_i64((int64_t)({value}), (size_t){line}, (size_t){column})"
            );
            finish_value(out, body, types, destination, target, &call)
        }
        "to_i64" => {
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("to_i64 result is {other:?}, not an IntResult enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::Int(IntTy::I64)])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("to_i64 result enum {eid} has no i64 Ok variant"),
                })?;
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("to_i64 result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = _parsed.value }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = ((ae_str){{ .ptr = _message, .len = (int64_t)strlen(_message), .owned = false }}) }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_f64_to_i64_result _parsed = ae_f64_to_i64_checked((double)({value}));\n      if (_parsed.status == 0) {{ {dest_expr} = {ok}; }}\n      else {{ const char *_message = _parsed.status == 1 ? \"f64 value is not finite\" : (_parsed.status == 2 ? \"f64 value is outside the i64 range\" : \"f64 value is not an exact integer\"); {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "parse_int" => {
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("parse_int result is {other:?}, not a Result enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::Int(IntTy::I64)])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("parse_int result enum {eid} has no i64 Ok variant"),
                })?;
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("parse_int result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = _parsed.value }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = ((ae_str){{ .ptr = _message, .len = (int64_t)strlen(_message), .owned = false }}) }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_parse_i64_result _parsed = ae_parse_i64({text});\n      if (_parsed.status == 0) {{ {dest_expr} = {ok}; }}\n      else {{ const char *_message = _parsed.status == 1 ? \"empty or sign-only integer\" : (_parsed.status == 2 ? \"not a base-10 integer\" : \"integer out of i64 range\"); {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "wrapping_add" | "wrapping_sub" | "wrapping_mul" | "wrapping_neg"
        | "saturating_add" | "saturating_sub" | "saturating_mul" => {
            let width = match types.infer_operand_ty(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a receiver"),
                })?,
            ) {
                Some(TyKind::Int(width)) => width,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} receiver is {other:?}, not an integer"),
                    });
                }
            };
            let prefix = match name {
                "wrapping_add" => "ae_wrap_add",
                "wrapping_sub" => "ae_wrap_sub",
                "wrapping_mul" => "ae_wrap_mul",
                "wrapping_neg" => "ae_wrap_neg",
                "saturating_add" => "ae_sat_add",
                "saturating_sub" => "ae_sat_sub",
                _ => "ae_sat_mul",
            };
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let call = if args.len() == 1 {
                format!("{prefix}_{}({a})", width.name())
            } else {
                let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
                format!("{prefix}_{}({a}, {b})", width.name())
            };
            finish_value(out, body, types, destination, target, &call)
        }
        "checked_add" | "checked_sub" | "checked_mul" | "checked_div" | "checked_rem"
        | "checked_neg" => {
            let width = match types.infer_operand_ty(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a receiver"),
                })?,
            ) {
                Some(TyKind::Int(width)) => width,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} receiver is {other:?}, not an integer"),
                    });
                }
            };
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} result is {other:?}, not an Option enum"),
                    });
                }
            };
            let prefix = match name {
                "checked_add" => "ae_opt_add",
                "checked_sub" => "ae_opt_sub",
                "checked_mul" => "ae_opt_mul",
                "checked_div" => "ae_opt_div",
                "checked_rem" => "ae_opt_mod",
                _ => "ae_opt_neg",
            };
            let c_w = width.c_name();
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let call = if args.len() == 1 {
                format!("{prefix}_{}({a}, &_opt)", width.name())
            } else {
                let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
                format!("{prefix}_{}({a}, {b}, &_opt)", width.name())
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let some = format!("((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _opt }} }} }})");
            let none = format!("((ae_enum_{eid}){{ .tag = 1 }})");
            let _ = writeln!(
                out,
                "    {{ {c_w} _opt = 0; if ({call}) {{ {dest_expr} = {some}; }} else {{ {dest_expr} = {none}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "count_lines_with" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "count_lines_with needs text and a pattern".to_owned(),
                });
            }
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let pattern = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_count_lines_with({text}, {pattern})"),
            )
        }
        "tokens" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "tokens needs the base text and a separator".to_owned(),
                });
            }
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let sep = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            // The cursor carries its call site: an empty separator reports
            // the tokenize call, not ae:0:0.
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_tokens_new_at({text}, {sep}, (size_t){line}, (size_t){column})"),
            )
        }
        "reset_tokens" => {
            let expr = cursor_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "reset_tokens needs a cursor".to_owned(),
                })?,
                types,
                def_map,
            )?;
            let _ = writeln!(out, "    ae_tokens_reset(&{expr});");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "advance_tokens" => {
            let expr = cursor_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "advance_tokens needs a cursor".to_owned(),
                })?,
                types,
                def_map,
            )?;
            finish_value(out, body, types, destination, target, &format!("ae_tokens_next(&{expr})"))
        }
        "tokens_current" => {
            let expr = cursor_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "tokens_current needs a cursor".to_owned(),
                })?,
                types,
                def_map,
            )?;
            finish_value(out, body, types, destination, target, &format!("ae_tokens_current(&{expr})"))
        }
        "atomic_new" => {
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_cell_new((int64_t)({value}))"))
        }
        "atomic_load" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_cell_load({handle})"))
        }
        "atomic_add" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let value = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_cell_add({handle}, (int64_t)({value}))"),
            )
        }
        "atomic_cas" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let expected = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let replacement = arg_exprs.get(2).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!(
                    "ae_cell_cas({handle}, (int64_t)({expected}), (int64_t)({replacement}))"
                ),
            )
        }
        "atomic_store" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let value = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let _ = writeln!(out, "    ae_cell_store({handle}, (int64_t)({value}));");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "chan_new" => {
            let cap = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_chan_new((int64_t)({cap}))"))
        }
        "chan_send" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let value = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_chan_send({handle}, (int64_t)({value}))"),
            )
        }
        "chan_try_send" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let value = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_chan_try_send({handle}, (int64_t)({value}))"),
            )
        }
        "chan_recv" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_chan_recv({handle})"))
        }
        "chan_close" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let _ = writeln!(out, "    ae_chan_close({handle});");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "chan_select" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_chan_select({a}, {b})"))
        }
        "now_s" => {
            finish_value(out, body, types, destination, target, "ae_now_s()")
        }
        "now_ms" => {
            finish_value(out, body, types, destination, target, "ae_now_ms()")
        }
        "args" => {
            match types.infer_place_ty(body, destination) {
                Some(TyKind::Vector(id)) if types.vector_elem(id) == Some(TyKind::String) => {
                    finish_value(out, body, types, destination, target, "ae_args_value()")
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("args() into {other:?} is not supported yet"),
                }),
            }
        }
        "read_file" => {
            let path = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            match dest_ty {
                Some(TyKind::String) => finish_value_releasing(
                    out,
                    body,
                    types,
                    destination,
                    target,
                    &format!("ae_read_file_value({path}, (size_t){line}, (size_t){column})"),
                    &string_temp_releases(body, args),
                ),
                Some(TyKind::Enum(_, eid)) => {
                    let dest_expr = emit_place(body, destination, types, dest_ty)?;
                    let ok = format!(
                        "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _read.text }} }} }})"
                    );
                    let err = format!(
                        "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _read.error }} }} }})"
                    );
                    let _ = writeln!(
                        out,
                        "    {{ ae_read_result _read = ae_try_read_file_value({path});\n      if (_read.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
                    );
                    for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                        let _ = writeln!(out, "    goto bb{};", next_bb.0);
                    } else {
                        out.push_str("    ae_trap(\"diverging call returned\");\n");
                    }
                    Ok(())
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("read_file into {other:?} is not supported yet"),
                }),
            }
        }
        "read_stdin" => {
            let dest_ty = types.infer_place_ty(body, destination);
            match dest_ty {
                Some(TyKind::Enum(_, eid)) => {
                    let dest_expr = emit_place(body, destination, types, dest_ty)?;
                    let ok = format!(
                        "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _read.text }} }} }})"
                    );
                    let err = format!(
                        "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _read.error }} }} }})"
                    );
                    let _ = writeln!(
                        out,
                        "    {{ ae_read_result _read = ae_try_read_stdin_value();\n      if (_read.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
                    );
                    for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                        let _ = writeln!(out, "    goto bb{};", next_bb.0);
                    } else {
                        out.push_str("    ae_trap(\"diverging call returned\");\n");
                    }
                    Ok(())
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("read_stdin into {other:?} is not supported yet"),
                }),
            }
        }
        "write_file" => {
            let path = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let content = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            match dest_ty {
                Some(TyKind::Int(_)) => finish_value_releasing(
                    out,
                    body,
                    types,
                    destination,
                    target,
                    &format!("ae_write_file_value({path}, {content}, (size_t){line}, (size_t){column})"),
                    &string_temp_releases(body, args),
                ),
                Some(TyKind::Enum(_, eid)) => {
                    let dest_expr = emit_place(body, destination, types, dest_ty)?;
                    let ok = format!(
                        "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _write.written }} }} }})"
                    );
                    let err = format!(
                        "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _write.error }} }} }})"
                    );
                    let _ = writeln!(
                        out,
                        "    {{ ae_write_result _write = ae_try_write_file_value({path}, {content});\n      if (_write.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
                    );
                    for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                        let _ = writeln!(out, "    goto bb{};", next_bb.0);
                    } else {
                        out.push_str("    ae_trap(\"diverging call returned\");\n");
                    }
                    Ok(())
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("write_file into {other:?} is not supported yet"),
                }),
            }
        }
        "path_join" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value_releasing(out, body, types, destination, target, &format!("ae_path_join_value({a}, {b})"), &string_temp_releases(body, args))
        }
        "path_name" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value_releasing(out, body, types, destination, target, &format!("ae_path_name_value({a})"), &string_temp_releases(body, args))
        }
        "path_parent" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value_releasing(out, body, types, destination, target, &format!("ae_path_parent_value({a})"), &string_temp_releases(body, args))
        }
        "path_exists" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_path_exists_value({a})"))
        }
        "path_is_dir" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_path_is_dir_value({a})"))
        }
        "list_dir" => {
            let path_expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("list_dir result is {other:?}, not a DirResult enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| matches!(fields.as_slice(), [TyKind::Vector(_)]))
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("list_dir result enum {eid} has no vector Ok variant"),
                })?;
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("list_dir result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = _dir.names }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = _dir.error }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_dir_result _dir = ae_try_list_dir_value({path_expr});\n      if (_dir.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "file_info" => {
            let path_expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("file_info result is {other:?}, not a FileResult enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| matches!(fields.as_slice(), [TyKind::Struct(_)]))
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("file_info result enum {eid} has no struct Ok variant"),
                })?;
            let sid = match layouts.get(ok_variant).and_then(|fields| fields.first()) {
                Some(TyKind::Struct(sid)) => *sid,
                _ => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("file_info Ok payload of enum {eid} is not a struct"),
                    });
                }
            };
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("file_info result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = ((ae_struct_{sid}){{ .f0 = _info.size, .f1 = _info.mtime, .f2 = _info.is_dir }}) }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = _info.error }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_file_info_result _info = ae_try_file_info_value({path_expr});\n      if (_info.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "run_command" => {
            let program = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let (args_ptr, elem) = vector_operand(
                body,
                args.get(1).ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "run_command needs an argument vector".to_owned(),
                })?,
                types,
                def_map,
            )?;
            if elem != TyKind::String {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("run_command arguments must be strings, found {elem:?}"),
                });
            }
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("run_command result is {other:?}, not a CommandResult enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| matches!(fields.as_slice(), [TyKind::Struct(_)]))
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("run_command result enum {eid} has no struct Ok variant"),
                })?;
            let sid = match layouts.get(ok_variant).and_then(|fields| fields.first()) {
                Some(TyKind::Struct(sid)) => *sid,
                _ => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("run_command Ok payload of enum {eid} is not a struct"),
                    });
                }
            };
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("run_command result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = ((ae_struct_{sid}){{ .f0 = _cmd.code, .f1 = _cmd.output }}) }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = _cmd.error }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_command_result _cmd = ae_run_command_value({program}, {args_ptr}, (size_t){line}, (size_t){column});\n      if (_cmd.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "json_get" => {
            let doc = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let path = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("json_get result is {other:?}, not a ReadResult enum"),
                    });
                }
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _json.u.v0.f0 }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _json.u.v1.f0 }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_json_read_result _json = ae_json_get({doc}, {path}, (size_t){line}, (size_t){column});\n      if (_json.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "json_int" | "json_len" => {
            let doc = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let path = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} result is {other:?}, not an IntResult enum"),
                    });
                }
            };
            let helper = if name == "json_len" { "ae_json_len" } else { "ae_json_int" };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _json.u.v0.f0 }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _json.u.v1.f0 }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_json_int_result _json = {helper}({doc}, {path}, (size_t){line}, (size_t){column});\n      if (_json.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "json_bool" => {
            let doc = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let path = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("json_bool result is {other:?}, not a BoolResult enum"),
                    });
                }
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _json.u.v0.f0 }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _json.u.v1.f0 }} }} }})"
            );
            let _ = writeln!(
                out,
                "    {{ ae_json_bool_result _json = ae_json_bool({doc}, {path}, (size_t){line}, (size_t){column});\n      if (_json.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "json_has" => {
            let doc = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let path = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value_releasing(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_json_has({doc}, {path}, (size_t){line}, (size_t){column})"),
                &string_temp_releases(body, args),
            )
        }
        "append_file" => {
            // `try_append_file` reports the byte count, so its WriteResult enum
            // shares the write_file shape rather than the status family.
            let path = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let content = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            match dest_ty {
                Some(TyKind::Int(_)) => finish_value_releasing(
                    out,
                    body,
                    types,
                    destination,
                    target,
                    &format!("ae_append_file_value({path}, {content}, (size_t){line}, (size_t){column})"),
                    &string_temp_releases(body, args),
                ),
                Some(TyKind::Enum(_, eid)) => {
                    let dest_expr = emit_place(body, destination, types, dest_ty)?;
                    let ok = format!(
                        "((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _write.written }} }} }})"
                    );
                    let err = format!(
                        "((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _write.error }} }} }})"
                    );
                    let _ = writeln!(
                        out,
                        "    {{ ae_write_result _write = ae_try_append_file_value({path}, {content});
      if (_write.status == 0) {{ {dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
                    );
                    for release in string_temp_releases(body, args) {
                        let _ = writeln!(out, "    {release};");
                    }
                    if let Some(next_bb) = target {
                        let _ = writeln!(out, "    goto bb{};", next_bb.0);
                    } else {
                        out.push_str("    ae_trap(\"diverging call returned\");
");
                    }
                    Ok(())
                }
                other => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("append_file into {other:?} is not supported yet"),
                }),
            }
        }
        "copy_file" | "rename_path" | "remove_file" | "remove_dir" | "create_dir"
        | "set_cwd" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let call = match name {
                "copy_file" => format!("ae_try_copy_file_value({a}, {b})"),
                "rename_path" => format!("ae_try_rename_path_value({a}, {b})"),
                "remove_file" => format!("ae_try_remove_file_value({a})"),
                "remove_dir" => format!("ae_try_remove_dir_value({a})"),
                "create_dir" => format!("ae_try_create_dir_value({a})"),
                _ => format!("ae_try_set_cwd_value({a})"),
            };
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} result is {other:?}, not a StatusResult enum"),
                    });
                }
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {{ ae_status_result _status = {call};\n      if (_status.status == 0) {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 0 }}); }} else {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _status.error }} }} }}); }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "env" => {
            let name_expr = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("env result is {other:?}, not a ReadResult enum"),
                    });
                }
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {{ ae_read_result _read = ae_try_env_value({name_expr});\n      if (_read.status == 0) {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _read.text }} }} }}); }} else {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _read.error }} }} }}); }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "cwd" => {
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("cwd result is {other:?}, not a ReadResult enum"),
                    });
                }
            };
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {{ ae_read_result _read = ae_try_cwd_value();\n      if (_read.status == 0) {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 0, .u = {{ .v0 = {{ .f0 = _read.text }} }} }}); }} else {{ {dest_expr} = ((ae_enum_{eid}){{ .tag = 1, .u = {{ .v1 = {{ .f0 = _read.error }} }} }}); }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "map_new" | "f64map_new" => {
            finish_value(out, body, types, destination, target, "ae_map_new()")
        }
        "map_put" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "map_put needs a map, a key and a value".to_owned(),
                });
            }
            let (map, _key_ty, value_ty) = map_operand(body, &args[0], types, def_map)?;
            let key = emit_operand(body, &args[1], types, def_map)?;
            let value = emit_operand(body, &args[2], types, def_map)?;
            match value_ty {
                TyKind::Int(_) => {
                    let _ = writeln!(out, "    ae_map_put(&{map}, {key}, (int64_t)({value}));");
                }
                TyKind::Float(FloatTy::F64) => {
                    let _ = writeln!(
                        out,
                        "    {{ double _v = {value}; int64_t _bits; memcpy(&_bits, &_v, sizeof _bits); ae_map_put(&{map}, {key}, _bits); }}"
                    );
                }
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("map values of {other:?} are not supported yet"),
                    });
                }
            }
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "map_get" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "map_get needs a map and a key".to_owned(),
                });
            }
            let (map, _key_ty, value_ty) = map_operand(body, &args[0], types, def_map)?;
            let key = emit_operand(body, &args[1], types, def_map)?;
            let dest_ty = types.infer_place_ty(body, destination);
            let eid = match dest_ty {
                Some(TyKind::Enum(_, id)) => id,
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("map_get result is {other:?}, not a Result enum"),
                    });
                }
            };
            let layouts = body.enum_layouts.get(&eid).ok_or_else(|| {
                CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("enum layout for id {eid} is not carried on the body"),
                }
            })?;
            let ok_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [value_ty])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("map_get result enum {eid} has no {value_ty:?} Ok variant"),
                })?;
            let err_variant = layouts
                .iter()
                .position(|fields| fields.as_slice() == [TyKind::String])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("map_get result enum {eid} has no string Err variant"),
                })?;
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let ok_value = match value_ty {
                TyKind::Int(_) => "_bits".to_owned(),
                TyKind::Float(FloatTy::F64) => "_value".to_owned(),
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("map values of {other:?} are not supported yet"),
                    });
                }
            };
            let ok = format!(
                "((ae_enum_{eid}){{ .tag = {ok_variant}, .u = {{ .v{ok_variant} = {{ .f0 = {ok_value} }} }} }})"
            );
            let err = format!(
                "((ae_enum_{eid}){{ .tag = {err_variant}, .u = {{ .v{err_variant} = {{ .f0 = ((ae_str){{ .ptr = \"key not found\", .len = (int64_t)(sizeof(\"key not found\") - 1), .owned = false }}) }} }} }})"
            );
            let (prefix, prepare) = if matches!(value_ty, TyKind::Float(_)) {
                ("int64_t _bits = 0; double _value = 0.0;", "memcpy(&_value, &_bits, sizeof _value); ")
            } else {
                ("int64_t _bits = 0;", "")
            };
            let _ = writeln!(
                out,
                "    {{ {prefix} if (ae_map_lookup(&{map}, {key}, &_bits)) {{ {prepare}{dest_expr} = {ok}; }} else {{ {dest_expr} = {err}; }} }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "map_has" => {
            let (map, _key_ty, _value_ty) = map_operand(body, &args[0], types, def_map)?;
            let key = emit_operand(body, &args[1], types, def_map)?;
            finish_value(out, body, types, destination, target, &format!("ae_map_has(&{map}, {key})"))
        }
        "map_remove" => {
            let (map, _key_ty, _value_ty) = map_operand(body, &args[0], types, def_map)?;
            let key = emit_operand(body, &args[1], types, def_map)?;
            finish_value(out, body, types, destination, target, &format!("ae_map_remove(&{map}, {key})"))
        }
        "map_inc" => {
            if args.len() != 3 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "map_inc needs a map, a key and a delta".to_owned(),
                });
            }
            let (map, _key_ty, value_ty) = map_operand(body, &args[0], types, def_map)?;
            let key = emit_operand(body, &args[1], types, def_map)?;
            let delta = emit_operand(body, &args[2], types, def_map)?;
            let call = match value_ty {
                TyKind::Int(_) => format!("ae_map_inc(&{map}, {key}, (int64_t)({delta}))"),
                TyKind::Float(FloatTy::F64) => {
                    format!("ae_map_inc_f64(&{map}, {key}, (double)({delta}))")
                }
                other => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("map values of {other:?} are not supported yet"),
                    });
                }
            };
            finish_value(out, body, types, destination, target, &call)
        }
        "keys" | "map_keys" => {
            let (map, key_ty, _value_ty) = map_operand(body, &args[0], types, def_map)?;
            if key_ty != TyKind::String {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("map keys of {key_ty:?} are not supported yet"),
                });
            }
            finish_value(out, body, types, destination, target, &format!("ae_map_keys(&{map})"))
        }
        "shared_new" => {
            let arg = args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: "shared_new needs a value".to_owned(),
            })?;
            let value_ty = types
                .infer_operand_ty(body, arg)
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "shared_new value has no inferable type".to_owned(),
                })?;
            let c_ty = c_type_name(value_ty);
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {{ {c_ty} _tmp = {value}; {dest_expr} = ae_shared_new(&_tmp, sizeof({c_ty})); }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "shared_get" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let c_ty = c_type_name(dest_ty.unwrap_or(TyKind::Int(IntTy::I64)));
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(out, "    {dest_expr} = (*(({c_ty} *)(({handle}).box)));");
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "shared_set" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "shared_set needs a handle and a value".to_owned(),
                });
            }
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let value_ty = types
                .infer_operand_ty(body, &args[1])
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "shared_set value has no inferable type".to_owned(),
                })?;
            let c_ty = c_type_name(value_ty);
            let value = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let _ = writeln!(
                out,
                "    {{ {c_ty} _tmp = {value}; *(({c_ty} *)(({handle}).box)) = _tmp; }}"
            );
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "mutex_new" => {
            finish_value(out, body, types, destination, target, "ae_mutex_new()")
        }
        "mutex_lock" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_mutex_lock({handle})"))
        }
        "exec_new" => {
            finish_value(out, body, types, destination, target, "ae_exec_new()")
        }
        "exec_run" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_exec_run({handle})"))
        }
        "task_yield" => {
            out.push_str("    ae_task_yield();\n");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "task_spawn" | "scope_spawn" | "pool_spawn" => {
            if args.len() < 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a handle and a callable"),
                });
            }
            let callable = match &args[1] {
                Operand::Constant(c) => match &c.value {
                    ConstValue::FnDef(id) => *id,
                    other => {
                        return Err(CodegenError::UnsupportedFeature {
                            body_name: body.name.clone(),
                            detail: format!("{name} callable constant {other:?} is not supported"),
                        });
                    }
                },
                _ => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: format!("{name} needs a closure literal"),
                    });
                }
            };
            if !types.task_spawns.contains_key(&callable) {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("no recorded context for task callable DefId({})", callable.0),
                });
            }
            let id = callable.0;
            let ctx = format!("ae_task_ctx_{id}");
            let entry = format!("ae_task_entry_{id}");
            let spawn_fn = match name {
                "task_spawn" => "ae_task_spawn",
                "scope_spawn" => "ae_scope_spawn",
                _ => "ae_pool_spawn",
            };
            let executor = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let mut text = format!(
                "    {{ {ctx} *_ctx = ({ctx} *)malloc(sizeof * _ctx);\n"
            );
            for (index, op) in args[2..].iter().enumerate() {
                let value = emit_operand(body, op, types, def_map)?;
                text.push_str(&format!("      _ctx->f{index} = {value};\n"));
            }
            text.push_str(&format!(
                "      {spawn_fn}({executor}, {entry}, (void *)_ctx); }}"
            ));
            let _ = writeln!(out, "{text}");
            finish_value(out, body, types, destination, target, "((ae_unit_t){ 0 })")
        }
        "scope_new" => {
            finish_value(out, body, types, destination, target, "ae_scope_new()")
        }
        "pool_new" => {
            let workers = arg_exprs.first().map(String::as_str).unwrap_or("1");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_pool_new((int64_t)({workers}))"),
            )
        }
        "pool_join" => {
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_pool_join({handle})"))
        }
        "thread_spawn" => {
            if args.is_empty() {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "thread_spawn needs a callable".to_owned(),
                });
            }
            let callable_ty = args.last().and_then(|op| types.infer_operand_ty(body, op));
            let dest_ty = types.infer_place_ty(body, destination);
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            // A `fn() -> T` value spawns through a signature-wide context:
            // the pointer rides the context and the trampoline calls through
            // it. A closure literal names its own lifted function instead.
            if let Some(TyKind::Fn(sig)) = callable_ty {
                let callable = args.last().map(|op| emit_operand(body, op, types, def_map)).transpose()?;
                let callable = callable.unwrap_or_else(|| "0".to_owned());
                let ctx = format!("ae_thread_fncall_{sig}");
                let entry = format!("ae_thread_entry_f{sig}");
                let mut text = format!("    {{ {ctx} *_ctx = ({ctx} *)malloc(sizeof * _ctx);\n");
                text.push_str(&format!("      _ctx->fn = {callable};\n"));
                text.push_str(&format!(
                    "      {dest_expr} = ae_thread_spawn_ctx({entry}, (void *)_ctx);\n      if (!{dest_expr}) ae_trap(\"out of memory\"); }}"
                ));
                let _ = writeln!(out, "{text}");
                if let Some(next_bb) = target {
                    let _ = writeln!(out, "    goto bb{};", next_bb.0);
                } else {
                    out.push_str("    ae_trap(\"diverging call returned\");\n");
                }
                return Ok(());
            }
            let callable = match args.last() {
                Some(Operand::Constant(c)) => match &c.value {
                    ConstValue::FnDef(id) => *id,
                    other => {
                        return Err(CodegenError::UnsupportedFeature {
                            body_name: body.name.clone(),
                            detail: format!("thread_spawn callable constant {other:?} is not supported"),
                        });
                    }
                },
                _ => {
                    return Err(CodegenError::UnsupportedFeature {
                        body_name: body.name.clone(),
                        detail: "thread_spawn needs a closure literal".to_owned(),
                    });
                }
            };
            if !types.thread_spawns.contains_key(&callable) {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("no recorded context for thread callable DefId({})", callable.0),
                });
            }
            let id = callable.0;
            let ctx = format!("ae_thread_ctx_{id}");
            let entry = format!("ae_thread_entry_{id}");
            let mut text = format!(
                "    {{ {ctx} *_ctx = ({ctx} *)malloc(sizeof * _ctx);\n"
            );
            for (index, op) in args[..args.len() - 1].iter().enumerate() {
                let value = emit_operand(body, op, types, def_map)?;
                text.push_str(&format!("      _ctx->f{index} = {value};\n"));
            }
            text.push_str(&format!(
                "      {dest_expr} = ae_thread_spawn_ctx({entry}, (void *)_ctx);\n      if (!{dest_expr}) ae_trap(\"out of memory\"); }}"
            ));
            let _ = writeln!(out, "{text}");
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "thread_wait" => {
            if args.len() != 1 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "thread_wait needs a handle".to_owned(),
                });
            }
            let handle = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let dest_ty = types.infer_place_ty(body, destination);
            let ret_c = c_type_name(dest_ty.unwrap_or(TyKind::Unit));
            let dest_expr = emit_place(body, destination, types, dest_ty)?;
            let _ = writeln!(
                out,
                "    {{ ae_thread_slot *_slot = {handle};\n      if (!_slot) ae_trap(\"use of a moved thread handle\");\n      if (!ae_thread_join(_slot->native)) ae_trap(\"thread join failed\");\n      {dest_expr} = *(({ret_c} *)_slot->ctx);\n      free(_slot->ctx);\n      free(_slot); }}"
            );
            for release in string_temp_releases(body, args) {
                let _ = writeln!(out, "    {release};");
            }
            if let Some(next_bb) = target {
                let _ = writeln!(out, "    goto bb{};", next_bb.0);
            } else {
                out.push_str("    ae_trap(\"diverging call returned\");\n");
            }
            Ok(())
        }
        "sqrt" | "floor" | "ceil" | "round" | "exp" | "log" | "sin" | "cos" | "abs" => {
            let arg = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let symbol = if name == "abs" { "fabs" } else { name };
            finish_value(out, body, types, destination, target, &format!("{symbol}((double)({arg}))"))
        }
        "pow" | "atan2" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let symbol = name;
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("(double)({symbol}((double)({a}), (double)({b})))"),
            )
        }
        "min" | "max" => {
            let a = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let b = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_f64_{name}((double)({a}), (double)({b}))"),
            )
        }
        "split" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "split needs text and a separator".to_owned(),
                });
            }
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let sep = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_vec_split({text}, {sep}, (size_t){line}, (size_t){column})"),
            )
        }
        "lines" => {
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_vec_lines({text})"))
        }
        "utf8_len" => {
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_utf8_len({text})"))
        }
        "utf8_prefix" => {
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            let count = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            finish_value(
                out,
                body,
                types,
                destination,
                target,
                &format!("ae_utf8_prefix({text}, (int64_t)({count}), (size_t){line}, (size_t){column})"),
            )
        }
        "trim" => {
            let text = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("ae_str_trim({text})"))
        }
        "ptr_offset" | "byte_offset" => {
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("{name} needs a pointer and a count"),
                });
            }
            let base = emit_operand(body, &args[0], types, def_map)?;
            let count = arg_exprs.get(1).map(String::as_str).unwrap_or("0");
            let call = if name == "byte_offset" {
                format!("((void *)((char *)({base}) + (int64_t)({count})))")
            } else {
                let pointee = match types.infer_operand_ty(body, &args[0]) {
                    Some(TyKind::Ptr(id) | TyKind::PtrMut(id)) => types.pointees.get(&id).copied(),
                    _ => None,
                };
                let pointee = pointee.ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "ptr_offset pointer has no known pointee type".to_owned(),
                })?;
                let c_pointee = c_type_name(pointee);
                format!(
                    "((void *)((char *)({base}) + (int64_t)({count}) * (int64_t)sizeof({c_pointee})))"
                )
            };
            finish_value(out, body, types, destination, target, &call)
        }
        "ptr_cast" | "ptr_cast_mut" => {
            // Both pointer kinds are `void *` in this backend, so a
            // reinterpretation is the same address with no runtime step.
            let value = arg_exprs.first().map(String::as_str).unwrap_or("0");
            finish_value(out, body, types, destination, target, &format!("((void *){value})"))
        }
        "ptr_drop_in_place" => {
            // The pointer is `void *`; the pointee type decides which release
            // helper runs, exactly like a drop of a local of that type. A
            // scalar pointee has nothing to release.
            let pointer = emit_operand(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "ptr_drop_in_place needs a pointer".to_owned(),
                })?,
                types,
                def_map,
            )?;
            let pointee = match types.infer_operand_ty(
                body,
                args.first().ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "ptr_drop_in_place needs a pointer".to_owned(),
                })?,
            ) {
                Some(TyKind::Ptr(id) | TyKind::PtrMut(id)) => types.pointees.get(&id).copied(),
                _ => None,
            };
            let call = match pointee {
                Some(TyKind::String) => format!("ae_str_release((ae_str *)({pointer}))"),
                Some(TyKind::Vector(_)) => format!("ae_vec_release((ae_vec *)({pointer}))"),
                Some(TyKind::MapOf(_)) => format!("ae_map_release((ae_map *)({pointer}))"),
                Some(TyKind::Struct(id)) if types.structs.contains_key(&id) => {
                    format!("ae_rel_struct_{id}((ae_struct_{id} *)({pointer}))")
                }
                Some(TyKind::Enum(_, id)) if types.enums.contains_key(&id) => {
                    format!("ae_rel_enum_{id}((ae_enum_{id} *)({pointer}))")
                }
                // A scalar (or an unknown pointee) has no owned state.
                _ => format!("((void)({pointer}))"),
            };
            finish_value(out, body, types, destination, target, &format!("({call}, (int64_t)0)"))
        }
        "array_index" => {
            // A fixed array's length is part of its type, so only an index the
            // frontend could not prove in range reaches this call (M2.10).
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "array_index needs an index and a length".to_owned(),
                });
            }
            let index = emit_operand(body, &args[0], types, def_map)?;
            let len = emit_operand(body, &args[1], types, def_map)?;
            let call = format!(
                "((int64_t)ae_array_index((int64_t)({index}), (size_t)({len}), (size_t){line}, (size_t){column}))"
            );
            finish_value(out, body, types, destination, target, &call)
        }
        "ptr_write" => {
            // `ptr_write(pointer, value)` stores through a raw pointer inside
            // `unsafe`. The pointee type selects the C cast, exactly like the
            // pointer-deref assignment the MIR lowering already produces for
            // `*p = v`.
            if args.len() != 2 {
                return Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: "ptr_write needs a pointer and a value".to_owned(),
                });
            }
            let pointer = emit_operand(body, &args[0], types, def_map)?;
            let value = emit_operand(body, &args[1], types, def_map)?;
            let pointee = match types.infer_operand_ty(body, &args[0]) {
                Some(TyKind::Ptr(id) | TyKind::PtrMut(id)) => types.pointees.get(&id).copied(),
                _ => None,
            };
            let c_pointee = pointee.map(c_type_name).unwrap_or_else(|| "int64_t".to_owned());
            let _ = writeln!(out, "    *(({c_pointee} *)({pointer})) = {value};");
            finish_value(out, body, types, destination, target, "((int64_t)0)")
        }
        other => Err(CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: format!("intrinsic '{other}' is not supported by the pure C backend yet"),
        }),
    }
}

/// Emit the release for a `Drop` statement or terminator. Types whose
/// ownership helpers have not landed yet stay comments (the pre-existing
/// behaviour); a dropped string is released exactly once.
fn emit_drop(
    out: &mut String,
    body: &Body,
    place: &Place,
    types: &ModuleTypes,
) -> Result<(), CodegenError> {
    let place_expr = emit_place(body, place, types, None)?;
    match types.infer_place_ty(body, place) {
        Some(TyKind::String) => {
            let _ = writeln!(out, "    ae_str_release(&{place_expr});");
        }
        Some(TyKind::Struct(id)) if types.structs.contains_key(&id) => {
            let _ = writeln!(out, "    ae_rel_struct_{id}(&{place_expr});");
        }
        Some(TyKind::Enum(_, id)) if types.enums.contains_key(&id) => {
            let _ = writeln!(out, "    ae_rel_enum_{id}(&{place_expr});");
        }
        Some(TyKind::Vector(_)) => {
            let _ = writeln!(out, "    ae_vec_release(&{place_expr});");
        }
        Some(TyKind::MapOf(_)) => {
            let _ = writeln!(out, "    ae_map_release(&{place_expr});");
        }
        Some(TyKind::Thread(_)) => {
            let _ = writeln!(out, "    ae_thread_release(&{place_expr});");
        }
        Some(TyKind::Guard) => {
            let _ = writeln!(out, "    ae_mutex_unlock(&{place_expr});");
        }
        Some(TyKind::TaskScope) => {
            let _ = writeln!(out, "    ae_scope_release(&{place_expr});");
        }
        Some(other) if other.owned() => {
            // Fail closed: a drop we cannot emit would silently leak.
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("drop of {other:?} is not supported by the pure C backend yet"),
            });
        }
        _ => {
            let _ = writeln!(out, "    /* drop {place_expr} */");
        }
    }
    Ok(())
}

/// Assign a value-producing intrinsic's result and continue at its target.
fn finish_value(
    out: &mut String,
    body: &Body,
    types: &ModuleTypes,
    destination: &Place,
    target: Option<ae_mir::BasicBlockId>,
    value: &str,
) -> Result<(), CodegenError> {
    let dest_ty = types.infer_place_ty(body, destination);
    let dest_expr = emit_place(body, destination, types, dest_ty)?;
    let _ = writeln!(out, "    {dest_expr} = {value};");
    if let Some(next_bb) = target {
        let _ = writeln!(out, "    goto bb{};", next_bb.0);
    } else {
        out.push_str("    ae_trap(\"diverging call returned\");\n");
    }
    Ok(())
}

/// Symbols the pure C backend supplies in its own runtime preamble. A
/// declaration naming anything else still fails closed.
fn runtime_extern(symbol: &str) -> bool {
    // `ae_exposure_trampoline` is the U7 rehearsal: a C caller reaching a
    // `#[no_mangle]` exposure through a foreign declaration. The runtime
    // defines it `static inline`, so a program that never calls it never
    // instantiates it — and a program that calls it without defining the
    // `test_add` exposure fails at link time, exactly like the legacy
    // backend's pruned trampoline.
    matches!(symbol, "ae_std_alloc" | "ae_std_free" | "ae_extern_probe" | "ae_exposure_trampoline")
}

/// String operands the lowering materialized as private temporaries are the
/// sole owner of their buffer; a read-only intrinsic does not take them, so
/// they are released right after the call. A zeroed (moved) slot is a no-op.
fn string_temp_releases(body: &Body, args: &[Operand]) -> Vec<String> {
    let mut releases = Vec::new();
    for op in args {
        if let Operand::Copy(place) = op
            && place.projection.is_empty()
            && let Some(decl) = body.local_decl(place.local)
            && decl.kind == ae_mir::LocalKind::Temp
            && matches!(decl.ty, TyKind::String | TyKind::StrView)
        {
            releases.push(format!("ae_str_release(&(_{}))", place.local.0));
        }
    }
    releases
}

/// Like [`finish_value`], but releases the given temporaries after the
/// result is stored and before the branch.
fn finish_value_releasing(
    out: &mut String,
    body: &Body,
    types: &ModuleTypes,
    destination: &Place,
    target: Option<ae_mir::BasicBlockId>,
    value: &str,
    releases: &[String],
) -> Result<(), CodegenError> {
    let dest_ty = types.infer_place_ty(body, destination);
    let dest_expr = emit_place(body, destination, types, dest_ty)?;
    let _ = writeln!(out, "    {dest_expr} = {value};");
    for release in releases {
        let _ = writeln!(out, "    {release};");
    }
    if let Some(next_bb) = target {
        let _ = writeln!(out, "    goto bb{};", next_bb.0);
    } else {
        out.push_str("    ae_trap(\"diverging call returned\");\n");
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
                Some(TyKind::SliceMut(elem_id)) => {
                    let elem =
                        types
                            .slice_elem(elem_id)
                            .ok_or_else(|| CodegenError::UnsupportedFeature {
                                body_name: body.name.clone(),
                                detail: format!(
                                    "slice element type for id {elem_id} is not carried on the body"
                                ),
                            })?;
                    let c_ty = c_type_name(elem);
                    expr = format!(
                        "(*(({c_ty} *)ae_slice_at_mut(&{expr}, (int64_t)_{}, (size_t)0, (size_t)0)))",
                        idx_local.0
                    );
                    cur_ty = Some(elem);
                }
                other => {
                    return Err(CodegenError::UnsupportedType {
                        body_name: body.name.clone(),
                        detail: format!("cannot index into {other:?}"),
                    });
                }
            },
            ProjectionElem::Deref => {
                // A reference deref knows its pointee from the type; a raw
                // pointer uses the type the caller inferred for the place.
                let reference_pointee = match cur_ty {
                    Some(TyKind::StructRef(sid) | TyKind::StructMut(sid)) => Some(TyKind::Struct(sid)),
                    Some(TyKind::VectorRef(id)) => Some(TyKind::Vector(id)),
                    Some(TyKind::Ptr(id) | TyKind::PtrMut(id)) => types.pointees.get(&id).copied(),
                    _ => None,
                };
                let target_ty = reference_pointee.or(if is_last { deref_pointee_ty } else { None });
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

/// Renders an operand that a consuming operation (concatenation, vector
/// element store) takes ownership of. A moved value transfers ownership; a
/// borrowed read (field, element, view) is copied into a fresh owned buffer
/// for strings so the consumer never touches the owner's bytes; a literal is
/// static and needs neither.
fn emit_owned_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    match op {
        Operand::Move(place) if types.infer_place_ty(body, place) == Some(TyKind::String) => {
            let expr = emit_place(body, place, types, Some(TyKind::String))?;
            Ok(format!("ae_str_take(&{expr})"))
        }
        Operand::Copy(place)
            if matches!(
                types.infer_place_ty(body, place),
                Some(TyKind::String | TyKind::StrView)
            ) =>
        {
            let expr = emit_place(body, place, types, None)?;
            Ok(format!("ae_str_clone({expr})"))
        }
        _ => emit_operand(body, op, types, def_map),
    }
}

/// Resolves the vector behind an operand: the C expression that points at its
/// header (`ae_vec *`) and its element type.
fn vector_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    _def_map: &BTreeMap<DefId, String>,
) -> Result<(String, TyKind), CodegenError> {
    let place = match op {
        Operand::Copy(place) | Operand::Move(place) => place,
        Operand::Constant(c) => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("vector operand cannot be a constant {:?}", c.value),
            });
        }
    };
    let vec_ty = types.infer_place_ty(body, place).ok_or_else(|| {
        CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: "vector operand has no inferable type".to_owned(),
        }
    })?;
    let (id, borrowed) = match vec_ty {
        TyKind::Vector(id) => (id, false),
        TyKind::VectorRef(id) => (id, true),
        other => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("expected a vector operand, found {other:?}"),
            });
        }
    };
    let elem = types.vector_elem(id).ok_or_else(|| CodegenError::UnsupportedFeature {
        body_name: body.name.clone(),
        detail: format!("vector element type for id {id} is not carried on the body"),
    })?;
    if !types.elem_supported(elem) {
        return Err(CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: format!("vector elements of {elem:?} are not supported by the pure C backend yet"),
        });
    }
    let expr = emit_place(body, place, types, Some(vec_ty))?;
    let ptr = if borrowed { expr } else { format!("(&{expr})") };
    Ok((ptr, elem))
}

/// Builds an `ae_slice` view over a place that holds a vector, an array, or
/// another slice, with the element type. The storage stays with the source.
fn slice_view_of_place(
    body: &Body,
    place: &Place,
    types: &ModuleTypes,
) -> Result<(String, TyKind), CodegenError> {
    let ty = types.infer_place_ty(body, place).ok_or_else(|| {
        CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: "slice source has no inferable type".to_owned(),
        }
    })?;
    let expr = emit_place(body, place, types, Some(ty))?;
    let (view, elem) = match ty {
        TyKind::Vector(id) | TyKind::VectorRef(id) => {
            let elem = types.vector_elem(id).ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("vector element type for id {id} is not carried on the body"),
            })?;
            let c_elem = c_type_name(elem);
            let arrow = if matches!(ty, TyKind::VectorRef(_)) { "->" } else { "." };
            (
                format!(
                    "ae_slice_view({expr}{arrow}data, {expr}{arrow}len, (int64_t)sizeof({c_elem}))"
                ),
                elem,
            )
        }
        TyKind::ArrayOf(elem_id, len) => {
            let elem = types
                .arrays
                .get(&(elem_id, len))
                .and_then(|t| *t)
                .ok_or_else(|| CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("array element type for ({elem_id}, {len}) is not known"),
                })?;
            let c_elem = c_type_name(elem);
            (
                format!("ae_slice_view({expr}.data, (int64_t){len}, (int64_t)sizeof({c_elem}))"),
                elem,
            )
        }
        TyKind::Slice(id) | TyKind::SliceMut(id) => {
            let elem = types.slice_elem(id).ok_or_else(|| CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("slice element type for id {id} is not carried on the body"),
            })?;
            (expr, elem)
        }
        other => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("slice source type {other:?} is not supported yet"),
            });
        }
    };
    Ok((view, elem))
}

/// Resolves the slice behind an operand: the C expression naming the view
/// and its element type.
fn slice_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    _def_map: &BTreeMap<DefId, String>,
) -> Result<(String, TyKind), CodegenError> {
    let place = match op {
        Operand::Copy(place) | Operand::Move(place) => place,
        Operand::Constant(c) => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("slice operand cannot be a constant {:?}", c.value),
            });
        }
    };
    slice_view_of_place(body, place, types)
}

/// Resolves the token cursor behind an operand.
fn cursor_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    _def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    let place = match op {
        Operand::Copy(place) | Operand::Move(place) => place,
        Operand::Constant(c) => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("token cursor cannot be a constant {:?}", c.value),
            });
        }
    };
    match types.infer_place_ty(body, place) {
        Some(TyKind::Tokens) => emit_place(body, place, types, Some(TyKind::Tokens)),
        other => Err(CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: format!("expected a token cursor operand, found {other:?}"),
        }),
    }
}

/// Resolves the map behind an operand: the map expression, its key type and
/// its value type.
fn map_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    _def_map: &BTreeMap<DefId, String>,
) -> Result<(String, TyKind, TyKind), CodegenError> {
    let place = match op {
        Operand::Copy(place) | Operand::Move(place) => place,
        Operand::Constant(c) => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("map operand cannot be a constant {:?}", c.value),
            });
        }
    };
    let ty = types.infer_place_ty(body, place).ok_or_else(|| {
        CodegenError::UnsupportedFeature {
            body_name: body.name.clone(),
            detail: "map operand has no inferable type".to_owned(),
        }
    })?;
    let id = match ty {
        TyKind::MapOf(id) => id,
        other => {
            return Err(CodegenError::UnsupportedFeature {
                body_name: body.name.clone(),
                detail: format!("expected a map operand, found {other:?}"),
            });
        }
    };
    let (key, value) = types.map_types(id).ok_or_else(|| CodegenError::UnsupportedFeature {
        body_name: body.name.clone(),
        detail: format!("map types for id {id} are not carried on the body"),
    })?;
    let expr = emit_place(body, place, types, Some(ty))?;
    Ok((expr, key, value))
}

fn emit_operand(
    body: &Body,
    op: &Operand,
    types: &ModuleTypes,
    def_map: &BTreeMap<DefId, String>,
) -> Result<String, CodegenError> {
    match op {
        Operand::Copy(place) => {
            let ty = types.infer_place_ty(body, place);
            emit_place(body, place, types, ty)
        }
        // A move transfers ownership: leave the source slot empty so its
        // conservative release (or any later one) frees nothing twice.
        Operand::Move(place) => {
            let ty = types.infer_place_ty(body, place);
            let expr = emit_place(body, place, types, ty)?;
            match ty {
                Some(TyKind::String) => Ok(format!("ae_str_take(&{expr})")),
                Some(TyKind::Struct(id)) if types.structs.contains_key(&id) => {
                    Ok(format!("ae_take_struct_{id}(&{expr})"))
                }
                Some(TyKind::Enum(_, id)) if types.enums.contains_key(&id) => {
                    Ok(format!("ae_take_enum_{id}(&{expr})"))
                }
                Some(TyKind::Vector(_)) => Ok(format!("ae_vec_take(&{expr})")),
                Some(TyKind::MapOf(_)) => Ok(format!("ae_map_take(&{expr})")),
                Some(TyKind::Thread(_)) => Ok(format!("ae_thread_take(&{expr})")),
                Some(TyKind::Guard) => Ok(format!("ae_guard_take(&{expr})")),
                Some(TyKind::TaskScope) => Ok(format!("ae_task_scope_take(&{expr})")),
                Some(other) if other.owned() => Err(CodegenError::UnsupportedFeature {
                    body_name: body.name.clone(),
                    detail: format!("moving a {other:?} value is not supported yet"),
                }),
                _ => Ok(expr),
            }
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
            // The literal body already uses C-compatible escapes (the lexer
            // validates the AE set, which is a subset); emitting it raw is
            // what the legacy backend does, so both produce the same bytes.
            // `sizeof` of that literal is the decoded byte length (escapes
            // already applied), and a literal is static: it is never freed.
            Ok(format!(
                "((ae_str){{ .ptr = \"{s}\", .len = (int64_t)(sizeof(\"{s}\") - 1), .owned = false }})"
            ))
        }
        (ConstValue::FnDef(def_id), TyKind::Fn(sig)) => {
            let name = def_map.get(def_id).ok_or_else(|| CodegenError::UnresolvedCall {
                body_name: body.name.clone(),
                def_id: *def_id,
            })?;
            // A function designator converts to the signature's typedef.
            Ok(format!("((ae_fn_t{sig}){name})"))
        }
        (ConstValue::FnDef(def_id), TyKind::Closure(_)) => {
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
    span: Option<ae_span::Span>,
) -> Result<String, CodegenError> {
    match rvalue {
        Rvalue::Use(op) => emit_operand(body, op, types, def_map),
        Rvalue::Ref { place, .. } | Rvalue::AddressOf(place) => {
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
                    let (line, column) = span.map(|s| (s.line, s.column)).unwrap_or((0, 0));
                    Ok(format!("ae_neg_{}({expr}, (size_t){line}, (size_t){column})", ity.name()))
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
            emit_binary(body, *op, ty, &l_expr, &r_expr, span)
        }
        Rvalue::Cast(kind, operand, target_ty) => {
            let expr = emit_operand(body, operand, types, def_map)?;
            let src_ty = types.infer_operand_ty(body, operand);
            let target_c_ty = c_type_name(*target_ty);
            if matches!(kind, ae_mir::CastKind::Misc) && matches!(src_ty, Some(TyKind::Bool)) {
                return Ok(format!("(({target_c_ty})(({expr}) ? 1 : 0))"));
            }
            // A float-to-integer cast truncates toward zero; non-finite and
            // out-of-range values trap instead of producing garbage (the
            // legacy runtime's per-width trunc contract).
            if matches!(src_ty, Some(TyKind::Float(_)))
                && let TyKind::Int(ity) = target_ty
            {
                let width = ity.name();
                let (line, column) = span
                    .as_ref()
                    .map(|s| (s.line, s.column))
                    .unwrap_or((0, 0));
                return Ok(format!(
                    "ae_f64_to_{width}_trunc((double)({expr}), (size_t){line}, (size_t){column})"
                ));
            }
            // An integer-to-char cast must land on a Unicode scalar value.
            if matches!(src_ty, Some(TyKind::Int(_))) && matches!(target_ty, TyKind::Char) {
                let (line, column) = span.as_ref().map(|s| (s.line, s.column)).unwrap_or((0, 0));
                return Ok(format!(
                    "ae_u32_to_char((uint32_t)({expr}), (size_t){line}, (size_t){column})"
                ));
            }
            Ok(format!("(({target_c_ty})({expr}))"))
        }
        Rvalue::Aggregate(kind, ops) => {
            let mut op_exprs = Vec::with_capacity(ops.len());
            for op in ops {
                op_exprs.push(emit_operand(body, op, types, def_map)?);
            }
            match kind {
                AggregateKind::Struct(id) => {
                    // A struct with a user drop keeps a live flag: a literal
                    // constructs it live, a move takes the flag along, and a
                    // release clears it before calling the user function.
                    let live = types.drop_fns.get(id).map(|(_, _, live)| live.as_str());
                    let mut fields: Vec<String> = op_exprs
                        .iter()
                        .enumerate()
                        .map(|(i, e)| format!(".f{i} = {e}"))
                        .collect();
                    if let Some(live) = live {
                        fields.push(format!(".{live} = true"));
                    }
                    if fields.is_empty() {
                        Ok(format!("((ae_struct_{id}){{ 0 }})"))
                    } else {
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
                AggregateKind::Array(_, _) => {
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
                    Ok(format!("((uintptr_t)ae_str_len({place_expr}))"))
                }
                Some(TyKind::Vector(_)) => {
                    let place_expr = emit_place(body, place, types, None)?;
                    Ok(format!("((uintptr_t)ae_vec_len(&{place_expr}))"))
                }
                Some(TyKind::VectorRef(_)) => {
                    let place_expr = emit_place(body, place, types, None)?;
                    Ok(format!("((uintptr_t)ae_vec_len({place_expr}))"))
                }
                Some(TyKind::MapOf(_)) => {
                    let place_expr = emit_place(body, place, types, None)?;
                    Ok(format!("((uintptr_t)ae_map_len(&{place_expr}))"))
                }
                Some(TyKind::Slice(_) | TyKind::SliceMut(_)) => {
                    let place_expr = emit_place(body, place, types, None)?;
                    Ok(format!("((uintptr_t)ae_slice_len({place_expr}))"))
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
    span: Option<ae_span::Span>,
) -> Result<String, CodegenError> {
    let (line, column) = span.map(|s| (s.line, s.column)).unwrap_or((0, 0));
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
                BinOp::Add => Ok(format!("ae_add_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Sub => Ok(format!("ae_sub_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Mul => Ok(format!("ae_mul_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Div => Ok(format!("ae_div_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Rem => Ok(format!("ae_rem_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Shl => Ok(format!("ae_shl_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
                BinOp::Shr => Ok(format!("ae_shr_{name}(({l}), ({r}), (size_t){line}, (size_t){column})")),
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
            // Zero is a reported failure, never a silent infinity.
            BinOp::Div => {
                let guard = match fty {
                    FloatTy::F32 => "ae_f32_div",
                    FloatTy::F64 => "ae_f64_div",
                };
                Ok(format!("{guard}(({l}), ({r}), (size_t){line}, (size_t){column})"))
            }
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
