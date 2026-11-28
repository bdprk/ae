//! Statement/expression emission to C.
use super::*;

// Expression emission tracks ownership of string values so every buffer is
// released exactly once: moved variables are nulled after hand-off, owned
// temporaries are consumed or freed by their statement, and releases are
// idempotent even if analysis and emission ever disagree.
#[derive(Debug)]
pub(super) struct Emitted {
    pub(super) text: String,
    pub(super) ty: Type,
    pub(super) owned: bool,
    pub(super) moves: Vec<(usize, Type)>,
    pub(super) materialized: bool,
}
impl Emitted {
    fn value(text: String, ty: Type) -> Self { Emitted { text, ty, owned: false, moves: Vec::new(), materialized: true } }
    fn moved(id: usize, ty: Type, text: String) -> Self { Emitted { text, ty, owned: true, moves: vec![(id, ty)], materialized: true } }
}

/// Whether a lowered string expression observes bytes owned elsewhere: a
/// struct field read, a string-vector element, a slice view, or a view
/// binding. Such a value is *copied* when a container takes it, never moved —
/// `push(v, p.name)` must not hand the vector the struct's bytes.
pub(super) fn string_expr_is_view(expr: &Expr) -> bool {
    match expr {
        Expr::StrVecIndex(..) | Expr::SliceView(..) => true,
        Expr::Variable(_, Type::StrView, ..) => true,
        Expr::Field(_, _, fty) => *fty == Type::String,
        _ => false,
    }
}

/// The C lvalue of a struct's field: `v{id}.field`, or `v{id}->field` when the
/// root is a mutable borrow (ADR-0185's through-pointer access).
pub(super) fn field_member(id: usize, field: &[(String, Type)], base_ty: Type) -> String {
    let dot = if matches!(base_ty, Type::StructMut(_)) { "->" } else { "." };
    let path = field.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(".");
    format!("v{id}{dot}{path}")
}

// Rust's Debug formatting is the shortest decimal that round-trips and always
// carries a '.' or an exponent, so it is a valid C double literal. Non-finite
// values never reach here: the lexer rejects them (ADR-37).
pub(super) fn c_float_literal(value: f64) -> String {
    let text = format!("{value:?}");
    if text.contains(['.', 'e', 'E']) { text } else { format!("{text}.0") }
}

pub(super) struct CodegenCtx<'a> {
    pub(super) ctx: &'a CompileContext,
    pub(super) structs: &'a [StructDef],
    pub(super) enums: &'a [EnumDef],
}
impl<'a> std::ops::Deref for CodegenCtx<'a> {
    type Target = CompileContext;
    fn deref(&self) -> &CompileContext { self.ctx }
}

fn emit_call_and_cleanup(callee: &str, args: &[Argument], ret: Type, out: &mut String, next: &mut usize, ctx: &CodegenCtx<'_>) -> Emitted {
    let mut moves = Vec::new();
    // A read-only argument only borrows its value, so a temporary
    // (literal or `+` result) stays owned by this expression and is
    // released after the call. Without this, `f("a" + b)` leaks.
    let mut view_temps: Vec<String> = Vec::new();
    let arguments = args.iter().filter_map(|arg| match arg {
        Argument::Value(e) => {
            let t = emit_expr(e, out, next, true, ctx);
            moves.extend(t.moves);
            (t.ty != Type::Unit).then_some(t.text)
        }
        Argument::Borrow(id, is_vec) => Some(if *is_vec { format!("&v{id}") } else { format!("v{id}") }),
        Argument::View(e) => {
            let t = emit_expr(e, out, next, false, ctx);
            moves.extend(t.moves);
            if t.owned { view_temps.push(t.text.clone()); }
            Some(format!("AE_VIEW({})", t.text))
        }
        // The function pointer goes into the argument list directly:
        // a temporary of function type would need C's parenthesized
        // declarator form, which the temp template cannot print.
        Argument::FnPtr(id, _) => Some(ctx.exposure_name(*id).unwrap_or_else(|| format!("ae_fn_{id}"))),
    }).collect::<Vec<_>>().join(", ");
    if ret == Type::Unit {
        out.push_str(&format!("    {callee}({arguments});\n"));
        for text in &view_temps { out.push_str(&format!("    ae_str_release(&{text});\n")); }
        null_moves(out, &moves, ctx);
        return Emitted { text: String::new(), ty: Type::Unit, owned: false, moves: Vec::new(), materialized: true };
    }
    let t = *next; *next += 1;
    out.push_str(&format!("    {}t{t} = {callee}({arguments});\n", ret.c_temp(ctx)));
    for text in &view_temps { out.push_str(&format!("    ae_str_release(&{text});\n")); }
    null_moves(out, &moves, ctx);
    Emitted { text: format!("t{t}"), ty: ret, owned: is_owned_type(ret, ctx.structs, ctx.enums), moves: Vec::new(), materialized: true }
}

pub(super) fn emit_expr(expr: &Expr, out: &mut String, next: &mut usize, moving: bool, ctx: &CodegenCtx<'_>) -> Emitted {
    let emitted = match expr {
        // spec/concurrency.md §8: copy-semantics atomics. The handle's C
        // struct copies by value, so every copy reaches the same cell; no
        // arm here moves or releases anything.
        Expr::AtomicNew(value) => {
            let v = emit_expr(value, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_atomic t{t} = ae_cell_new({});\n", v.text));
            Emitted { text: format!("t{t}"), ty: Type::Atomic, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::AtomicLoad(handle) => {
            let a = emit_expr(handle, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_cell_load({});\n", a.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::AtomicAdd(handle, value) => {
            let a = emit_expr(handle, out, next, false, ctx);
            let v = emit_expr(value, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_cell_add({}, {});\n", a.text, v.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::AtomicCas(handle, expect, new) => {
            let a = emit_expr(handle, out, next, false, ctx);
            let e = emit_expr(expect, out, next, false, ctx);
            let n = emit_expr(new, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    bool t{t} = ae_cell_cas({}, {}, {});\n", a.text, e.text, n.text));
            Emitted { text: format!("t{t}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        // ADR-0216, spec/concurrency.md §9: copy-semantics shared box handles.
        Expr::SharedNew(value, sid) => {
            let v = emit_expr(value, out, next, false, ctx);
            let elem = ctx.shared_types_get(*sid);
            let elem_c = c_type_field(elem, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_shared t{t} = ae_shared_alloc(sizeof({elem_c}));\n"));
            out.push_str(&format!("    *({elem_c} *)t{t}.box = {};\n", v.text));
            Emitted { text: format!("t{t}"), ty: Type::Shared(*sid), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::SharedGet(handle, sid) => {
            let h = emit_expr(handle, out, next, false, ctx);
            let elem = ctx.shared_types_get(*sid);
            let elem_c = c_type_field(elem, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    {elem_c} t{t} = *({elem_c} *)({}.box);\n", h.text));
            Emitted { text: format!("t{t}"), ty: elem, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::MutexNew => {
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_mutex t{t} = ae_mutex_new();\n"));
            Emitted { text: format!("t{t}"), ty: Type::Mutex, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::MutexLock(handle) => {
            let m = emit_expr(handle, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_guard t{t} = ae_mutex_lock({});\n", m.text));
            Emitted { text: format!("t{t}"), ty: Type::Guard, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::ChanNew(cap) => {
            let c = emit_expr(cap, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_chan t{t} = ae_chan_new({});\n", c.text));
            Emitted { text: format!("t{t}"), ty: Type::Chan, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ChanSend(ch, val) => {
            let c = emit_expr(ch, out, next, false, ctx);
            let v = emit_expr(val, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    bool t{t} = ae_chan_send({}, {});\n", c.text, v.text));
            Emitted { text: format!("t{t}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ChanTrySend(ch, val) => {
            let c = emit_expr(ch, out, next, false, ctx);
            let v = emit_expr(val, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    bool t{t} = ae_chan_try_send({}, {});\n", c.text, v.text));
            Emitted { text: format!("t{t}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ChanRecv(ch) => {
            let c = emit_expr(ch, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_chan_recv({});\n", c.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ChanSelect(c1, c2) => {
            let a = emit_expr(c1, out, next, false, ctx);
            let b = emit_expr(c2, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_chan_select({}, {});\n", a.text, b.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ExecNew => {
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_exec t{t} = ae_exec_new();\n"));
            Emitted { text: format!("t{t}"), ty: Type::Exec, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ExecRun(ex) => {
            let e = emit_expr(ex, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_exec_run({});\n", e.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ScopeNew => {
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_task_scope t{t} = ae_scope_new();\n"));
            Emitted { text: format!("t{t}"), ty: Type::TaskScope, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::PoolNew(n) => {
            let n = emit_expr(n, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    ae_pool t{t} = ae_pool_new({});\n", n.text));
            Emitted { text: format!("t{t}"), ty: Type::Pool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::PoolJoin(p) => {
            let p = emit_expr(p, out, next, false, ctx);
            let t = *next; *next += 1;
            out.push_str(&format!("    int64_t t{t} = ae_pool_join({});\n", p.text));
            Emitted { text: format!("t{t}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        // M2.12 slice 2: the address of addressable storage. The operand is a
        // variable or a field of one, so the C is an address-of; the pointer
        // itself is a plain value that neither moves nor releases.
        Expr::BorrowAs(operand, ..) => {
            let t = emit_expr(operand, out, next, false, ctx);
            Emitted::value(format!("&({})", t.text), crate::frontend::mir::expr_type(expr, ctx))
        }
        Expr::PtrCast(operand, mutable, pointee_id, ..) => {
            // ADR-0209 step A: a pointer reinterpretation — the same address
            // through a new pointee's eyes. Nothing happens at runtime; the
            // C cast is the whole story.
            let inner = emit_expr(operand, out, next, false, ctx);
            let pointee = ctx.ptr_pointee(*pointee_id);
            let ptr_c = if *mutable {
                format!("{} *", c_type_field(pointee, ctx))
            } else {
                format!("const {} *", c_type_field(pointee, ctx))
            };
            let target_ty = if *mutable { Type::PtrMut(*pointee_id) } else { Type::Ptr(*pointee_id) };
            let cast_arg = if inner.ty == Type::Int(IntTy::Usize) {
                format!("(uintptr_t)({})", inner.text)
            } else {
                format!("({})", inner.text)
            };
            Emitted::value(format!("({ptr_c}){cast_arg}"), target_ty)
        }
        Expr::PtrNull(pointee_id, ..) => {
            // `ptr::null[T]()`: the pointee only shapes the C cast; the value
            // is a plain null pointer constant.
            let pointee = ctx.ptr_pointee(*pointee_id);
            let ptr_c = format!("{} *", c_type_field(pointee, ctx));
            Emitted::value(format!("({ptr_c})0"), Type::PtrMut(*pointee_id))
        }
        Expr::PtrIsNull(pointer) => {
            let inner = emit_expr(pointer, out, next, false, ctx);
            Emitted::value(format!("(({}) == NULL)", inner.text), Type::Bool)
        }
        Expr::PtrWrite(pointer, value, ..) => {
            let pointer = emit_expr(pointer, out, next, false, ctx);
            let address = *next; *next += 1;
            out.push_str(&format!("    {}t{address} = {};\n", pointer.ty.c_temp(ctx), pointer.text));
            let value = emit_expr(value, out, next, true, ctx);
            out.push_str(&format!("    AE_PTR_WRITE(t{address}, {});\n", value.text));
            null_moves(out, &value.moves, ctx);
            Emitted::value("(int64_t)0".to_owned(), Type::Int(IntTy::I64))
        }
        Expr::PtrDropInPlace(pointer, pointee, ..) => {
            let p = emit_expr(pointer, out, next, false, ctx);
            let call = match pointee {
                Type::String => format!("(ae_str_release({}), (int64_t)0)", p.text),
                Type::Vector(id) => format!("({}_release({}), (int64_t)0)", c_output::vector_family(ctx, *id), p.text),
                Type::MapOf(id) => format!("({}_release({}), (int64_t)0)", c_output::map_family(ctx, *id), p.text),
                Type::Struct(sid) => format!("(ae_t{sid}_release((ae_t{sid} *)({})), (int64_t)0)", p.text),
                Type::Enum(_, eid) => format!("(ae_e{eid}_release((ae_e{eid} *)({})), (int64_t)0)", p.text),
                _ => format!("((void)({}), (int64_t)0)", p.text),
            };
            let result = *next; *next += 1;
            out.push_str(&format!("    int64_t t{result} = {call};\n"));
            Emitted::value(format!("t{result}"), Type::Int(IntTy::I64))
        }
        Expr::PtrOffset(pointer, count, bytewise, ..) => {
            // ADR-0209 step A: pointer arithmetic. The element form rides C's
            // own scaling (base is a T*, so base + n advances n elements);
            // the byte form goes through char* and back. Both keep the
            // base's pointer type.
            let base = emit_expr(pointer, out, next, false, ctx);
            let cnt = emit_expr(count, out, next, false, ctx);
            let base_ty = crate::frontend::mir::expr_type(pointer, ctx);
            let ptr_c = match base_ty {
                Type::Ptr(id) | Type::PtrMut(id) => format!("{} *", c_type_field(ctx.ptr_pointee(id), ctx)),
                _ => "int64_t *".to_owned(),
            };
            let text = if *bytewise {
                format!("({ptr_c})(((char *)({})) + (int64_t)({}))", base.text, cnt.text)
            } else {
                format!("({ptr_c})(({}) + (int64_t)({}))", base.text, cnt.text)
            };
            Emitted::value(text, base_ty)
        }
        Expr::DerefPtr(pointer, line, column) => {
            let t = emit_expr(pointer, out, next, false, ctx);
            let ty = crate::frontend::mir::expr_type(expr, ctx);
            // A view-typed deref (ADR-0245): the pointee is owned, so the
            // read must not copy the owning header. A string view copies the
            // ptr/len pair (AE_VIEW); a vector view materializes the slot's
            // address as a borrow pointer — `const family *tN` — which every
            // downstream consumer (element reads, the moved-vector guard)
            // takes directly; the len call dereferences per its family's
            // signature at the call site.
            if ty == Type::StrView {
                Emitted::value(format!("AE_VIEW(AE_PTR_READ({}))", t.text), ty)
            } else if let Type::VectorRef(elem_id) = ty {
                let family = c_output::vector_family(ctx, elem_id);
                let id = *next; *next += 1;
                out.push_str(&format!("    const {family} *t{id} = ((const {family} *)({}));\n", t.text));
                out.push_str(&format!("    if (!t{id}->data) ae_fail(\"use of moved vector\", {line}, {column});\n"));
                Emitted { text: format!("t{id}"), ty: Type::VectorRef(elem_id), owned: false, moves: Vec::new(), materialized: true }
            } else if let Type::MapRef(map_id) = ty {
                // A heap slot's map reads as the borrow pointer (ADR-0245):
                // materialize it like the vector view, with the slots
                // sentinel as the moved guard.
                let family = c_output::map_family(ctx, map_id);
                let id = *next; *next += 1;
                out.push_str(&format!("    const {family} *t{id} = ((const {family} *)({}));\n", t.text));
                out.push_str(&format!("    if (!t{id}->slots) ae_fail(\"use of moved map\", {line}, {column});\n"));
                Emitted { text: format!("t{id}"), ty: Type::MapRef(map_id), owned: false, moves: Vec::new(), materialized: true }
            } else {
                Emitted::value(format!("AE_PTR_READ({})", t.text), ty)
            }
        }
        Expr::Number(n) => Emitted::value(format!("INT64_C({n})"), Type::Int(IntTy::I64)),
        Expr::FloatLit(x) => Emitted::value(c_float_literal(*x), Type::Float(FloatTy::F64)),
        Expr::CharLit(c) => Emitted::value(format!("UINT32_C({})", *c as u32), Type::Char),
        Expr::Unit => Emitted::value(String::new(), Type::Unit),
        Expr::IntMethod(kind, receiver, arg, result_ty, _, _) => {
            let recv = emit_expr(receiver, out, next, false, ctx);
            let Type::Int(width) = recv.ty else { unreachable!("IntMethod receiver must be integer") };
            let w = width.name();
            let rhs = arg.as_ref().map(|a| emit_expr(a, out, next, false, ctx));
            let id = *next;
            *next += 1;
            match kind {
                IntMethodKind::WrappingAdd
                | IntMethodKind::WrappingSub
                | IntMethodKind::WrappingMul
                | IntMethodKind::WrappingNeg
                | IntMethodKind::SaturatingAdd
                | IntMethodKind::SaturatingSub
                | IntMethodKind::SaturatingMul => {
                    let prefix = match kind {
                        IntMethodKind::WrappingAdd => "ae_wrap_add",
                        IntMethodKind::WrappingSub => "ae_wrap_sub",
                        IntMethodKind::WrappingMul => "ae_wrap_mul",
                        IntMethodKind::WrappingNeg => "ae_wrap_neg",
                        IntMethodKind::SaturatingAdd => "ae_sat_add",
                        IntMethodKind::SaturatingSub => "ae_sat_sub",
                        IntMethodKind::SaturatingMul => "ae_sat_mul",
                        _ => unreachable!(),
                    };
                    if let Some(r) = rhs {
                        out.push_str(&format!("    {}t{id} = {prefix}_{w}({}, {});\n", result_ty.c_temp(ctx), recv.text, r.text));
                    } else {
                        out.push_str(&format!("    {}t{id} = {prefix}_{w}({});\n", result_ty.c_temp(ctx), recv.text));
                    }
                }
                IntMethodKind::CheckedAdd
                | IntMethodKind::CheckedSub
                | IntMethodKind::CheckedMul
                | IntMethodKind::CheckedDiv
                | IntMethodKind::CheckedRem
                | IntMethodKind::CheckedNeg => {
                    let Type::Enum(_, eid) = *result_ty else { unreachable!("checked_* must return Option[T]") };
                    let prefix = match kind {
                        IntMethodKind::CheckedAdd => "ae_opt_add",
                        IntMethodKind::CheckedSub => "ae_opt_sub",
                        IntMethodKind::CheckedMul => "ae_opt_mul",
                        IntMethodKind::CheckedDiv => "ae_opt_div",
                        IntMethodKind::CheckedRem => "ae_opt_mod",
                        IntMethodKind::CheckedNeg => "ae_opt_neg",
                        _ => unreachable!(),
                    };
                    let val_id = *next;
                    *next += 1;
                    out.push_str(&format!("    {} r{val_id} = 0;\n", width.c_name()));
                    out.push_str(&format!("    ae_e{eid} t{id};\n"));
                    let call = if let Some(r) = rhs {
                        format!("{prefix}_{w}({}, {}, &r{val_id})", recv.text, r.text)
                    } else {
                        format!("{prefix}_{w}({}, &r{val_id})", recv.text)
                    };
                    out.push_str(&format!("    if ({call}) {{\n"));
                    out.push_str(&format!("        t{id} = (ae_e{eid}){{ AE_E{eid}_0, {{ .v0 = {{ r{val_id} }} }} }};\n"));
                    out.push_str("    } else {\n");
                    out.push_str(&format!("        t{id} = (ae_e{eid}){{ AE_E{eid}_1 }};\n"));
                    out.push_str("    }\n");
                }
            }
            Emitted { text: format!("t{id}"), ty: *result_ty, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ScalarCast(value, from, to, line, column) => {
            let v = emit_expr(value, out, next, false, ctx);
            let id = *next; *next += 1;
            match (from, to) {
                // Int -> float rounds to nearest (IEEE); no trap.
                (Type::Int(_), Type::Float(FloatTy::F64)) => {
                    out.push_str(&format!("    double t{id} = (double)({});\n", v.text));
                }
                (Type::Int(_), Type::Float(FloatTy::F32)) => {
                    out.push_str(&format!("    float t{id} = (float)({});\n", v.text));
                }
                (Type::Float(FloatTy::F64), Type::Float(FloatTy::F32)) => {
                    out.push_str(&format!("    float t{id} = (float)({});\n", v.text));
                }
                (Type::Float(FloatTy::F32), Type::Float(FloatTy::F64)) => {
                    out.push_str(&format!("    double t{id} = (double)({});\n", v.text));
                }
                // Float -> int truncates and traps outside the target range or
                // on non-finite values, identically to the interpreter.
                (Type::Float(FloatTy::F64), Type::Int(width)) => {
                    let helper = format!("ae_f64_to_{}_trunc", width.name());
                    out.push_str(&format!("    {}t{id} = {helper}({}, {line}, {column});\n", to.c_temp(ctx), v.text));
                }
                (Type::Float(FloatTy::F32), Type::Int(width)) => {
                    let helper = format!("ae_f32_to_{}_trunc", width.name());
                    out.push_str(&format!("    {}t{id} = {helper}({}, {line}, {column});\n", to.c_temp(ctx), v.text));
                }
                (Type::Int(IntTy::U32), Type::Char) => {
                    out.push_str(&format!("    uint32_t t{id} = ae_u32_to_char({}, {line}, {column});\n", v.text));
                }
                _ => out.push_str(&format!("    {}t{id} = ({}) ({});\n", to.c_temp(ctx), to.c_temp(ctx).trim_end(), v.text)),
            }
            Emitted { text: format!("t{id}"), ty: *to, owned: false, moves: v.moves, materialized: true }
        }
        Expr::ToF64(value, line, column) => {
            let t = emit_expr(value, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    double t{id} = ae_f64_from_i64({}, {line}, {column});\n", t.text));
            Emitted { text: format!("t{id}"), ty: Type::Float(FloatTy::F64), owned: false, moves: t.moves, materialized: true }
        }
        Expr::ToI64(value, ty, line, column) => {
            let t = emit_expr(value, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_f64_to_i64({}, {line}, {column});\n", ty.c_temp(ctx), t.text));
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::MathCall(index, args, _line, _column) => {
            // Direct libm call: <math.h> is already in every generated C (the
            // float formatter needs it), no runtime function or pruning root
            // is involved. f64 arguments own nothing, so no move tracking.
            let c_symbol = MATH_FNS[*index as usize].1;
            let mut parts = Vec::new();
            for arg in args {
                let t = emit_expr(arg, out, next, false, ctx);
                parts.push(t.text);
            }
            let id = *next; *next += 1;
            out.push_str(&format!("    double t{id} = {c_symbol}({});\n", parts.join(", ")));
            Emitted { text: format!("t{id}"), ty: Type::Float(FloatTy::F64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::FloatToString(value, line, column) => {
            let t = emit_expr(value, out, next, false, ctx);
            let id = *next; *next += 1;
            if t.ty == Type::Float(FloatTy::F32) {
                out.push_str(&format!("    ae_string t{id} = ae_f64_to_string((double)({}), {line}, {column});\n", t.text));
            } else {
                out.push_str(&format!("    ae_string t{id} = ae_f64_to_string({}, {line}, {column});\n", t.text));
            }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::BoolLit(b) => Emitted::value(if *b { "true" } else { "false" }.to_owned(), Type::Bool),
        Expr::StrLit(text, line, column) => Emitted {
            text: format!("ae_str_from(\"{text}\", {line}, {column})"),
            ty: Type::String, owned: true, moves: Vec::new(), materialized: false,
        },
        Expr::Variable(id, ty, line, column) => {
            if let Type::VectorRef(elem_id) = *ty {
                out.push_str(&format!("    if (!v{id}->data) ae_fail(\"use of moved vector\", {line}, {column});\n"));
                Emitted { text: format!("(*v{id})"), ty: Type::Vector(elem_id), owned: false, moves: Vec::new(), materialized: true }
            } else if let Type::StructRef(sid) | Type::StructMut(sid) = *ty {
                // A struct borrow reads through the pointer (ADR-0184); the
                // mutable form reads the same way (ADR-0185) and writes go
                // through the field-store statement instead. The
                // dereferenced shape is the owned struct, so every consumer
                // downstream — field access, views of string fields — works
                // unchanged. There is no sentinel to check: the pointer is
                // never null, because only an address can create the borrow.
                Emitted { text: format!("(*v{id})"), ty: Type::Struct(sid), owned: false, moves: Vec::new(), materialized: true }
            } else if is_owned_type(*ty, ctx.structs, ctx.enums) {
                // Runtime backstop: a moved value's slot is empty.
                match ty {
                    // Structs and enums have no runtime sentinel: the compile-time
                    // move set is their only guard (documented limitation).
                    Type::Struct(_) | Type::Enum(_, _) => {}
                    // A thread handle or mutex guard is a plain handle: no runtime
                    // string/vec sentinel backstop here.
                    Type::Thread(_) | Type::Guard | Type::TaskScope => {}
                    _ => {
                        let probe = match ty { Type::Vector(_) => format!("!v{id}.data"), Type::MapOf(_) => format!("!v{id}.slots"), _ => format!("!v{id}.ptr") };
                        let message = match ty { Type::Vector(_) => "use of moved vector", Type::MapOf(_) => "use of moved map", _ => "use of moved string" };
                        out.push_str(&format!("    if ({probe}) ae_fail(\"{message}\", {line}, {column});\n"));
                    }
                }
                if moving {
                    let t = *next; *next += 1;
                    out.push_str(&format!("    {}t{t} = v{id};\n", ty.c_temp(ctx)));
                    Emitted::moved(*id, *ty, format!("t{t}"))
                } else {
                    Emitted { text: format!("v{id}"), ty: *ty, owned: false, moves: Vec::new(), materialized: true }
                }
            } else {
                Emitted::value(format!("v{id}"), *ty)
            }
        }
        Expr::Call(id, args, ret) | Expr::FnCall(id, args, ret, _) => {
            let callee = if matches!(expr, Expr::FnCall(..)) {
                format!("v{id}")
            } else {
                ctx.extern_name(*id).unwrap_or_else(|| format!("ae_fn_{id}"))
            };
            return emit_call_and_cleanup(&callee, args, *ret, out, next, ctx);
        }
        Expr::FnRef(fn_id, sig_id) => {
            let callee = ctx.exposure_name(*fn_id).unwrap_or_else(|| format!("ae_fn_{fn_id}"));
            Emitted::value(callee, Type::Fn(*sig_id))
        }
        Expr::FnExprCall(callee_expr, args, ret, _) => {
            let c = emit_expr(callee_expr, out, next, false, ctx);
            let callee = if c.materialized {
                c.text
            } else {
                let cid = *next;
                *next += 1;
                out.push_str(&format!("    {}t{cid} = {};\n", c.ty.c_temp(ctx), c.text));
                format!("t{cid}")
            };
            return emit_call_and_cleanup(&callee, args, *ret, out, next, ctx);
        }
        Expr::VecLit(values, vector_ty, line, column) => {
            let t = *next; *next += 1;
            let elem_ty = match *vector_ty {
                Type::Vector(id) => ctx.vector_elem(id),
                _ => Type::Int(IntTy::I64),
            };
            let is_gvec = matches!(*vector_ty, Type::Vector(id) if c_output::vector_family(ctx, id) == "ae_gvec");
            if is_gvec {
                let c_elem = c_output::c_type_field(elem_ty, ctx);
                let drop_fn = match *vector_ty {
                    Type::Vector(id) => c_output::gvec_drop_expr(ctx, id, ctx.structs, ctx.enums),
                    _ => "NULL".to_owned(),
                };
                out.push_str(&format!("    ae_gvec t{t} = ae_gvec_new((int64_t)sizeof({c_elem}), {drop_fn}, {line}, {column});\n"));
                let moving = is_owned_type(elem_ty, ctx.structs, ctx.enums);
                for value in values {
                    let element = emit_expr(value, out, next, moving, ctx);
                    let e = *next; *next += 1;
                    out.push_str(&format!("    {c_elem} t{e} = {};\n", element.text));
                    out.push_str(&format!("    ae_gvec_push(&t{t}, &t{e}, {line}, {column});\n"));
                    null_moves(out, &element.moves, ctx);
                }
            } else if elem_ty == Type::String {
                // A string literal vector: elements that are views are copied
                // into the vector, owned strings move in (ADR-0235).
                out.push_str(&format!("    ae_strvec t{t} = ae_strvec_new({line}, {column});\n"));
                for value in values {
                    let view = string_expr_is_view(value);
                    let element = emit_expr(value, out, next, !view, ctx);
                    let text = if view {
                        format!("ae_str_copy_view(AE_VIEW({}), {line}, {column})", element.text)
                    } else {
                        element.text.clone()
                    };
                    out.push_str(&format!("    ae_strvec_push(&t{t}, {text}, {line}, {column});\n"));
                    null_moves(out, &element.moves, ctx);
                }
            } else {
                // Only the string family has its own literal form left.
                unreachable!("vector literal family: every non-string element uses ae_gvec")
            }
            Emitted { text: format!("t{t}"), ty: *vector_ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::VecIndex(id, index, line, col, by_ref, elem_ty) => {
            let check = if *by_ref {
                format!("if (!v{id}->data) ae_fail(\"use of moved vector\", {line}, {col});")
            } else {
                format!("if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});")
            };
            out.push_str(&format!("    {check}\n"));
            let i = emit_expr(index, out, next, false, ctx);
            let c_elem = c_output::c_type_field(*elem_ty, ctx);
            let ptr_recv = if *by_ref { format!("v{id}") } else { format!("&v{id}") };
            let get = format!("(*((const {c_elem} *)ae_gvec_at({ptr_recv}, {}, {line}, {col})))", i.text);
            Emitted { text: get, ty: *elem_ty, owned: false, moves: i.moves, materialized: true }
        }
        Expr::VecIndexOf(base, index, line, col, elem_ty) => {
            // An element read through a field receiver (`r.cells[0]`). The
            // base is an lvalue, so the pointer accessors take its address;
            // the by-value accessors copy the vector header. A deref-chain
            // base (ADR-0245) is already the borrow pointer: access through
            // it directly.
            let b = emit_expr(base, out, next, false, ctx);
            let view_ptr = matches!(b.ty, Type::VectorRef(_));
            let guard = if view_ptr {
                format!("if (!({})->data) ae_fail(\"use of moved vector\", {line}, {col});\n", b.text)
            } else {
                format!("    if (!({}).data) ae_fail(\"use of moved vector\", {line}, {col});\n", b.text)
            };
            out.push_str(&guard);
            let i = emit_expr(index, out, next, false, ctx);
            let ptr = if view_ptr { b.text.clone() } else { format!("&({})", b.text) };
            let get = if *elem_ty == Type::String {
                format!("(*ae_strvec_get_const({ptr}, {}, {line}, {col}))", i.text)
            } else {
                let c_elem = c_output::c_type_field(*elem_ty, ctx);
                format!("(*((const {c_elem} *)ae_gvec_at({ptr}, {}, {line}, {col})))", i.text)
            };
            Emitted { text: get, ty: *elem_ty, owned: false, moves: i.moves, materialized: true }
        }
        Expr::SliceOf(slot, source, elem, line, col) => {
            // ADR-0236: the fat pointer is built at the call site. A vector
            // hands its storage; an array is already a pointer plus its
            // compile-time length; a slice forwards its value.
            let c_elem = c_output::c_type_field(*elem, ctx);
            let text = match *source {
                Type::Vector(_) => {
                    out.push_str(&format!("    if (!v{slot}.data) ae_fail(\"use of moved vector\", {line}, {col});
"));
                    format!("(ae_slice){{ v{slot}.data, v{slot}.len, (int64_t)sizeof({c_elem}) }}")
                }
                Type::VectorRef(_) => {
                    out.push_str(&format!("    if (!v{slot}->data) ae_fail(\"use of moved vector\", {line}, {col});
"));
                    format!("(ae_slice){{ v{slot}->data, v{slot}->len, (int64_t)sizeof({c_elem}) }}")
                }
                Type::ArrayOf(_, len) => format!("(ae_slice){{ (const void *)v{slot}, {len}, (int64_t)sizeof({c_elem}) }}"),
                Type::Slice(_) => format!("v{slot}"),
                _ => unreachable!("slice source kind"),
            };
            Emitted { text, ty: slice_type(ctx, *elem), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::SliceIndex(slot, index, line, col, elem) => {
            let i = emit_expr(index, out, next, false, ctx);
            let c_elem = c_output::c_type_field(*elem, ctx);
            let get = format!("(*((const {c_elem} *)ae_slice_at(v{slot}, {}, {line}, {col})))", i.text);
            Emitted { text: get, ty: *elem, owned: false, moves: i.moves, materialized: true }
        }
        Expr::SliceRange(slot, source, elem, start, end, line, col) => {
            // ADR-0236 rung 3: the bounds evaluate first, then the whole
            // slice is built in place and clipped by the runtime helper
            // (which owns the bounds check and names the call site).
            let st = emit_expr(start, out, next, false, ctx);
            let en = emit_expr(end, out, next, false, ctx);
            let c_elem = c_output::c_type_field(*elem, ctx);
            let whole = match *source {
                Type::Vector(_) => {
                    out.push_str(&format!("    if (!v{slot}.data) ae_fail(\"use of moved vector\", {line}, {col});"));
                    out.push('\n');
                    format!("(ae_slice){{ v{slot}.data, v{slot}.len, (int64_t)sizeof({c_elem}) }}")
                }
                Type::VectorRef(_) => {
                    out.push_str(&format!("    if (!v{slot}->data) ae_fail(\"use of moved vector\", {line}, {col});"));
                    out.push('\n');
                    format!("(ae_slice){{ v{slot}->data, v{slot}->len, (int64_t)sizeof({c_elem}) }}")
                }
                Type::ArrayOf(_, len) => format!("(ae_slice){{ (const void *)v{slot}, {len}, (int64_t)sizeof({c_elem}) }}"),
                Type::Slice(_) => format!("v{slot}"),
                _ => unreachable!("slice source kind"),
            };
            let text = format!("ae_slice_sub({whole}, {}, {}, {line}, {col})", st.text, en.text);
            let mut moves = st.moves;
            moves.extend(en.moves);
            Emitted { text, ty: slice_type(ctx, *elem), owned: false, moves, materialized: true }
        }
        Expr::Index(id, len, index, line, col, elem_ty) => {
            // A constant index inside the array's compile-time length is
            // provably in bounds: the check elides (M2.10). An out-of-range
            // constant keeps the runtime check and its trap.
            if let Expr::Number(n) = &**index {
                if *n >= 0 && (*n as usize) < *len {
                    Emitted { text: format!("v{id}[{n}]"), ty: *elem_ty, owned: false, moves: Vec::new(), materialized: false }
                } else {
                    let i = emit_expr(index, out, next, false, ctx);
                    Emitted { text: format!("v{id}[ae_index({}, {len}, {line}, {col})]", i.text), ty: *elem_ty, owned: false, moves: i.moves, materialized: false }
                }
            } else {
                let i = emit_expr(index, out, next, false, ctx);
                Emitted { text: format!("v{id}[ae_index({}, {len}, {line}, {col})]", i.text), ty: *elem_ty, owned: false, moves: i.moves, materialized: false }
            }
        }
        Expr::Len(e, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let id = *next; *next += 1;
            let call = match t.ty {
                Type::String | Type::StrView => format!("ae_str_len(AE_VIEW({}), {line}, {col})", t.text),
                Type::Vector(elem_id) => format!("{}_len({}, {line}, {col})", c_output::vector_family(ctx, elem_id), t.text),
                // A view operand is the family pointer; the i64 family's len
                // takes the header by value, the other families the pointer.
                Type::VectorRef(elem_id) => {
                    let family = c_output::vector_family(ctx, elem_id);
                    if family == "ae_gvec" {
                        format!("ae_gvec_len(*{}, {line}, {col})", t.text)
                    } else {
                        format!("{}_len({}, {line}, {col})", family, t.text)
                    }
                }
                Type::Slice(_) | Type::SliceMut(_) => format!("ae_slice_len({}, {line}, {col})", t.text),
                Type::MapOf(id) => format!("{}_len(&{}, {line}, {col})", c_output::map_family(ctx, id), t.text),
                _ => format!("ae_gvec_len({}, {line}, {col})", t.text),
            };
            out.push_str(&format!("    int64_t t{id} = {call};\n"));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Neg(a, line, column) => {
            let t = emit_expr(a, out, next, false, ctx);
            match t.ty {
                Type::Float(FloatTy::F64) | Type::Float(FloatTy::F32) => Emitted { text: format!("-({})", t.text), ty: t.ty, owned: false, moves: t.moves, materialized: false },
                Type::Int(IntTy::I64) => Emitted { text: format!("ae_neg({}, {line}, {column})", t.text), ty: Type::Int(IntTy::I64), owned: false, moves: t.moves, materialized: false },
                Type::Int(width) if width.signed() => {
                    Emitted { text: format!("ae_neg_{}({}, {line}, {column})", width.name(), t.text), ty: t.ty, owned: false, moves: t.moves, materialized: false }
                }
                other => unreachable!("unsupported neg type: {other:?}"),
            }
        }
        Expr::Not(a) => {
            let t = emit_expr(a, out, next, false, ctx);
            Emitted { text: format!("!{}", t.text), ty: Type::Bool, owned: false, moves: t.moves, materialized: false }
        }
        Expr::ReadFile(e, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_read_file({}.ptr, {line}, {col});
", t.text));
            // The path is only read: an owned temporary is released here, a
            // named slot keeps owning its buffer.
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::WriteFile(path, content, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let c = emit_expr(content, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    int64_t t{id} = ae_write_file({}, AE_VIEW({}), {line}, {col});
", p.text, c.text));
            // Both operands are read, never consumed by the call.
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if c.owned { drop_owned_temp(out, &c, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Bytes(e, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_gvec t{id} = ae_bytes(AE_VIEW({}), {line}, {col});
", t.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: vector_type(ctx, Type::Int(IntTy::I64)), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Slice(e, start, end, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let st = emit_expr(start, out, next, false, ctx);
            let en = emit_expr(end, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_str_slice(AE_VIEW({}), {}, {}, {line}, {col});
", t.text, st.text, en.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Contains(haystack, needle, line, col) => {
            let h = emit_expr(haystack, out, next, false, ctx);
            let n = emit_expr(needle, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    bool t{id} = ae_contains(AE_VIEW({}), AE_VIEW({}), {line}, {col});
", h.text, n.text));
            if h.owned { drop_owned_temp(out, &h, ctx); }
            if n.owned { drop_owned_temp(out, &n, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Find(haystack, needle, ty, line, col) => {
            let h = emit_expr(haystack, out, next, false, ctx);
            let n = emit_expr(needle, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_find(AE_VIEW({}), AE_VIEW({}), {line}, {col});\n", ty.c_temp(ctx), h.text, n.text));
            if n.owned { drop_owned_temp(out, &n, ctx); }
            if h.owned { drop_owned_temp(out, &h, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::StartsWith(s, prefix) => {
            let h = emit_expr(s, out, next, false, ctx);
            let p = emit_expr(prefix, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    bool t{id} = ae_starts_with(AE_VIEW({}), AE_VIEW({}));\n", h.text, p.text));
            if h.owned { drop_owned_temp(out, &h, ctx); }
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::EndsWith(s, suffix) => {
            let h = emit_expr(s, out, next, false, ctx);
            let p = emit_expr(suffix, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    bool t{id} = ae_ends_with(AE_VIEW({}), AE_VIEW({}));\n", h.text, p.text));
            if h.owned { drop_owned_temp(out, &h, ctx); }
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Trim(s, line, col) => {
            let t = emit_expr(s, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_trim(AE_VIEW({}), {line}, {col});\n", t.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Split(s, sep, line, col) => {
            let h = emit_expr(s, out, next, false, ctx);
            let n = emit_expr(sep, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_strvec t{id} = ae_split(AE_VIEW({}), AE_VIEW({}), {line}, {col});\n", h.text, n.text));
            if h.owned { drop_owned_temp(out, &h, ctx); }
            if n.owned { drop_owned_temp(out, &n, ctx); }
            Emitted { text: format!("t{id}"), ty: vector_type(ctx, Type::String), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Args(line, col) => {
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_strvec t{id} = ae_args({line}, {col});\n"));
            Emitted { text: format!("t{id}"), ty: vector_type(ctx, Type::String), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::StrVecIndex(id, index, line, col, by_ref) => {
            let (check, access) = if *by_ref {
                (format!("if (!v{id}->data) ae_fail(\"use of moved vector\", {line}, {col});"), format!("ae_strvec_get_const(v{id}"))
            } else {
                (format!("if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});"), format!("ae_strvec_get(&v{id}"))
            };
            out.push_str(&format!("    {check}\n"));
            let i = emit_expr(index, out, next, false, ctx);
            // Elements are borrow-only: the pointer view never owns.
            Emitted { text: format!("(*{access}, {}, {line}, {col}))", i.text), ty: Type::String, owned: false, moves: i.moves, materialized: true }
        }
        Expr::SliceView(vid, field, start, end, line, col) => {
            // The base is a borrowed string (a variable, or a string field of a
            // struct variable); it cannot move, be reassigned, or have that
            // field written while the view exists (borrow tracker).
            let access = match field { Some(name) => format!("v{vid}.{name}"), None => format!("v{vid}") };
            let st = emit_expr(start, out, next, false, ctx);
            let en = emit_expr(end, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    if (!{access}.ptr) ae_fail(\"use of moved string\", {line}, {col});\n"));
            out.push_str(&format!("    ae_strview t{id} = ae_slice_view(AE_VIEW({access}), {}, {}, {line}, {col});\n", st.text, en.text));
            Emitted { text: format!("t{id}"), ty: Type::StrView, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Tokens(vid, field, sep, line, col) => {
            // The cursor only remembers a view of the base and the separator;
            // it allocates nothing, so it needs no release (ADR-53).
            let access = match field { Some(name) => format!("v{vid}.{name}"), None => format!("v{vid}") };
            let sp = emit_expr(sep, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    if (!{access}.ptr) ae_fail(\"use of moved string\", {line}, {col});\n"));
            out.push_str(&format!("    ae_tokens t{id} = ae_tokens_new(AE_VIEW({access}), AE_VIEW({}), {line}, {col});\n", sp.text));
            // The cursor copies the separator bytes, so the temporary can be
            // released here without leaving a dangling pointer (ADR-53).
            if sp.owned { drop_owned_temp(out, &sp, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Tokens, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Join(vid, sep, line, col) => {
            out.push_str(&format!("    if (!v{vid}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
            let n = emit_expr(sep, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_join(&v{vid}, AE_VIEW({}), {line}, {col});\n", n.text));
            if n.owned { drop_owned_temp(out, &n, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Lines(s, line, col) => {
            let t = emit_expr(s, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_strvec t{id} = ae_lines(AE_VIEW({}), {line}, {col});
", t.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: vector_type(ctx, Type::String), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::CountLinesWith(s, pat, line, col) => {
            let t = emit_expr(s, out, next, false, ctx);
            let pt = emit_expr(pat, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    int64_t t{id} = ae_count_lines_with(AE_VIEW({}), AE_VIEW({}), {line}, {col});\n", t.text, pt.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            if pt.owned { drop_owned_temp(out, &pt, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::StructLit(sid, values) => {
            let mut parts = Vec::new();
            let mut moves = Vec::new();
            // A unit field's expression emits no text; the C member still
            // needs an initializer (an empty slot would be a syntax error).
            let fields = &ctx.structs[*sid].fields;
            for (index, v) in values.iter().enumerate() {
                let e = emit_expr(v, out, next, true, ctx);
                moves.extend(e.moves);
                if fields.get(index).is_some_and(|(_, ty)| *ty == Type::Unit) {
                    parts.push("0".to_owned());
                } else {
                    parts.push(e.text);
                }
            }
            let id = *next; *next += 1;
            let init = if parts.is_empty() { "0".to_owned() } else { parts.join(", ") };
            out.push_str(&format!(
                "    ae_t{sid} t{id} = {{ {init} }};\n    AE_T{sid}_ACTIVATE(t{id});\n",
            ));
            null_moves(out, &moves, ctx);
            Emitted { text: format!("t{id}"), ty: Type::Struct(*sid), owned: is_owned_type(Type::Struct(*sid), ctx.structs, ctx.enums), moves: Vec::new(), materialized: true }
        }
        Expr::EnumLit(_name, eid, vidx, values) => {
            let mut parts = Vec::new();
            let mut moves = Vec::new();
            // A unit payload has no value to emit; its C field still needs an
            // initializer, and `{  }` is rejected by `-pedantic-errors`.
            let fields = &ctx.enums[*eid].variants[*vidx].1;
            for (index, value) in values.iter().enumerate() {
                let e = emit_expr(value, out, next, true, ctx);
                moves.extend(e.moves);
                if fields.get(index).is_some_and(|ty| *ty == Type::Unit) {
                    parts.push("0".to_owned());
                } else {
                    parts.push(e.text);
                }
            }
            let id = *next; *next += 1;
            let payload = if parts.is_empty() { "{ 0 }".to_owned() } else { format!("{{ .v{vidx} = {{ {} }} }}", parts.join(", ")) };
            out.push_str(&format!("    ae_e{eid} t{id} = {{ AE_E{eid}_{vidx}, {payload} }};\n"));
            null_moves(out, &moves, ctx);
            Emitted { text: format!("t{id}"), ty: Type::Enum(_name, *eid), owned: is_owned_type(Type::Enum(_name, *eid), ctx.structs, ctx.enums), moves: Vec::new(), materialized: true }
        }
        Expr::TryReadFile(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_read_file({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::ReadFileBytes(path, offset, limit, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let o = emit_expr(offset, out, next, false, ctx);
            let l = emit_expr(limit, out, next, false, ctx);
            let id = *next; *next += 1;
            let emitted = match ty {
                Type::Enum(_, eid) => {
                    // The try form builds the enum in place: the error string
                    // is a static message from the runtime (never released),
                    // and the byte vector moves into the Ok variant (on the
                    // Err path it is the empty vector the runtime produced).
                    let chunk = id;
                    let err = id + 1;
                    let _ = next; *next += 2;
                    out.push_str(&format!(
                        "    ae_gvec t{chunk};\n    const char *t{err} = ae_read_file_bytes_into({p}.ptr, {o}, {l}, &t{chunk});\n",
                        chunk = chunk, err = err, p = p.text, o = o.text, l = l.text));
                    let result_id = *next; *next += 1;
                    out.push_str(&format!(
                        "    {}t{result_id} = t{err} ? (ae_e{eid}) {{ AE_BYTES_ERR, {{ .v1 = {{ ae_str_from(t{err}, {line}, {col}) }} }} }} : (ae_e{eid}) {{ AE_BYTES_OK, {{ .v0 = {{ t{chunk} }} }} }};\n",
                        ty.c_temp(ctx), eid = eid, err = err, chunk = chunk, line = line, col = col));
                    Emitted { text: format!("t{result_id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
                }
                _ => {
                    out.push_str(&format!("    ae_gvec t{id} = ae_read_file_bytes({p}.ptr, {o}, {l}, {line}, {col});\n", id = id, p = p.text, o = o.text, l = l.text, line = line, col = col));
                    Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
                }
            };
            if p.owned { drop_owned_temp(out, &p, ctx); }
            emitted
        }
        Expr::TryWriteFile(path, content, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let c = emit_expr(content, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_write_file({}.ptr, AE_VIEW({}), {line}, {col});\n", ty.c_temp(ctx), p.text, c.text));
            if c.owned { drop_owned_temp(out, &c, ctx); }
            // The path may be an owned temporary too (path_join of two views,
            // for instance); it is consumed by the call, not by the caller.
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryAppendFile(path, content, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let c = emit_expr(content, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_append_file({}.ptr, AE_VIEW({}), {line}, {col});\n", ty.c_temp(ctx), p.text, c.text));
            if c.owned { drop_owned_temp(out, &c, ctx); }
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::JsonGet(doc, path, ty, line, col) => {
            let d = emit_expr(doc, out, next, false, ctx);
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_json_get({}.ptr, {}.len, {}.ptr, {}.len, {line}, {col});\n", ty.c_temp(ctx), d.text, d.text, p.text, p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if d.owned { drop_owned_temp(out, &d, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::JsonInt(doc, path, ty, line, col) => {
            let d = emit_expr(doc, out, next, false, ctx);
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_json_int({}.ptr, {}.len, {}.ptr, {}.len, {line}, {col});\n", ty.c_temp(ctx), d.text, d.text, p.text, p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if d.owned { drop_owned_temp(out, &d, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::JsonBool(doc, path, ty, line, col) => {
            let d = emit_expr(doc, out, next, false, ctx);
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_json_bool({}.ptr, {}.len, {}.ptr, {}.len, {line}, {col});\n", ty.c_temp(ctx), d.text, d.text, p.text, p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if d.owned { drop_owned_temp(out, &d, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::JsonLen(doc, path, ty, line, col) => {
            let d = emit_expr(doc, out, next, false, ctx);
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_json_len({}.ptr, {}.len, {}.ptr, {}.len, {line}, {col});\n", ty.c_temp(ctx), d.text, d.text, p.text, p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if d.owned { drop_owned_temp(out, &d, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::JsonHas(doc, path, line, col) => {
            let d = emit_expr(doc, out, next, false, ctx);
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    const bool t{id} = ae_json_has({}.ptr, {}.len, {}.ptr, {}.len, {line}, {col});\n", d.text, d.text, p.text, p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            if d.owned { drop_owned_temp(out, &d, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Env(variable, ty, line, col) => {
            let v = emit_expr(variable, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_env({}.ptr, {line}, {col});\n", ty.c_temp(ctx), v.text));
            if v.owned { drop_owned_temp(out, &v, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::RunCommand(program, vid, ty, line, col) => {
            out.push_str(&format!("    if (!v{vid}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
            let p = emit_expr(program, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_run_command({}.ptr, &v{vid}, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::F64VecNew(line, col) => Emitted {
            text: format!("ae_gvec_new((int64_t)sizeof(double), NULL, {line}, {col})"),
            ty: vector_type(ctx, Type::Float(FloatTy::F64)), owned: true, moves: Vec::new(), materialized: true
        },
        Expr::StrVecNew(line, col) => Emitted {
            text: format!("ae_strvec_new({line}, {col})"),
            ty: vector_type(ctx, Type::String), owned: true, moves: Vec::new(), materialized: false,
        },
        Expr::ReadStdin(ty, line, col) => {
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_read_stdin({line}, {col});\n", ty.c_temp(ctx)));
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Cwd(ty, line, col) => {
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_cwd({line}, {col});\n", ty.c_temp(ctx)));
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::NowMs => {
            let id = *next; *next += 1;
            out.push_str(&format!("    int64_t t{id} = ae_now_ms();\n"));
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::NowS => {
            let id = *next; *next += 1;
            out.push_str(&format!("    int64_t t{id} = ae_now_s();\n"));
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::TryListDir(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_list_dir({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::PathJoin(dir, name, line, col) => {
            let d = emit_expr(dir, out, next, false, ctx);
            let n = emit_expr(name, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_path_join(AE_VIEW({}), AE_VIEW({}), {line}, {col});\n", d.text, n.text));
            if d.owned { drop_owned_temp(out, &d, ctx); }
            if n.owned { drop_owned_temp(out, &n, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::PathName(path, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_path_name(AE_VIEW({}), {line}, {col});\n", p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::PathParent(path, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_path_parent(AE_VIEW({}), {line}, {col});\n", p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryFileInfo(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_file_info({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryCopyFile(from, to, ty, line, col) => {
            let a = emit_expr(from, out, next, false, ctx);
            let b = emit_expr(to, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_copy_file({}.ptr, {}.ptr, {line}, {col});\n", ty.c_temp(ctx), a.text, b.text));
            if a.owned { drop_owned_temp(out, &a, ctx); }
            if b.owned { drop_owned_temp(out, &b, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryRenamePath(from, to, ty, line, col) => {
            let a = emit_expr(from, out, next, false, ctx);
            let b = emit_expr(to, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_rename_path({}.ptr, {}.ptr, {line}, {col});\n", ty.c_temp(ctx), a.text, b.text));
            if a.owned { drop_owned_temp(out, &a, ctx); }
            if b.owned { drop_owned_temp(out, &b, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryRemoveFile(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_remove_file({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::MapNew(line, col, kind) => {
            let id = *next; *next += 1;
            let (c_ty, new_fn, val_ty) = if *kind == MapKind::Float {
                ("ae_mapf", "ae_mapf_new", Type::Float(FloatTy::F64))
            } else {
                ("ae_map", "ae_map_new", Type::Int(IntTy::I64))
            };
            let ty = Type::MapOf(ctx.find_map_types(Type::String, val_ty).expect("pre-interned map types"));
            out.push_str(&format!("    {c_ty} t{id} = {new_fn}({line}, {col});\n"));
            Emitted { text: format!("t{id}"), ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::MapGet(place, key, ty, line, col) => {
            let k = emit_expr(key, out, next, false, ctx);
            let id = *next; *next += 1;
            let get_fn = if matches!(ty, Type::Enum(name, _) if *name == "FloatResult") { "ae_mapf_get" } else { "ae_map_get" };
            out.push_str(&format!("    {}t{id} = {get_fn}(&{}, AE_VIEW({}), {line}, {col});\n", ty.c_temp(ctx), place.text(), k.text));
            if k.owned { drop_owned_temp(out, &k, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::MapHas(place, key, line, col) => {
            let k = emit_expr(key, out, next, false, ctx);
            let id = *next; *next += 1;
            let has_fn = if place.f64_value() { "ae_mapf_has" } else { "ae_map_has" };
            out.push_str(&format!("    bool t{id} = {has_fn}(&{}, AE_VIEW({}), {line}, {col});\n", place.text(), k.text));
            if k.owned { drop_owned_temp(out, &k, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::MapInc(place, key, delta, line, col) => {
            let k = emit_expr(key, out, next, false, ctx);
            let d = emit_expr(delta, out, next, false, ctx);
            let id = *next; *next += 1;
            let (inc_fn, c_ty, ty) = if place.f64_value() {
                ("ae_mapf_inc", "double", Type::Float(FloatTy::F64))
            } else {
                ("ae_map_inc", "int64_t", Type::Int(IntTy::I64))
            };
            out.push_str(&format!("    {c_ty} t{id} = {inc_fn}(&{}, AE_VIEW({}), {}, {line}, {col});\n", place.text(), k.text, d.text));
            if d.owned { drop_owned_temp(out, &d, ctx); }
            if k.owned { drop_owned_temp(out, &k, ctx); }
            Emitted { text: format!("t{id}"), ty, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::MapRemove(place, key, line, col) => {
            let k = emit_expr(key, out, next, false, ctx);
            let id = *next; *next += 1;
            let remove_fn = if place.f64_value() { "ae_mapf_remove" } else { "ae_map_remove" };
            out.push_str(&format!("    bool t{id} = {remove_fn}(&{}, AE_VIEW({}), {line}, {col});\n", place.text(), k.text));
            if k.owned { drop_owned_temp(out, &k, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::MapKeys(place, line, col) => {
            let id = *next; *next += 1;
            let keys_fn = if place.f64_value() { "ae_mapf_keys" } else { "ae_map_keys" };
            out.push_str(&format!("    ae_strvec t{id} = {keys_fn}(&{}, {line}, {col});\n", place.text()));
            Emitted { text: format!("t{id}"), ty: vector_type(ctx, Type::String), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryRemoveDir(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_remove_dir({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::TryCreateDir(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_try_create_dir({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::SetCwd(path, ty, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_set_cwd({}.ptr, {line}, {col});\n", ty.c_temp(ctx), p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Utf8Len(e, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    int64_t t{id} = ae_utf8_len(AE_VIEW({}), {line}, {col});\n", t.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Int(IntTy::I64), owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Utf8Prefix(e, count, line, col) => {
            let t = emit_expr(e, out, next, false, ctx);
            let n = emit_expr(count, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    ae_string t{id} = ae_utf8_prefix(AE_VIEW({}), {}, {line}, {col});\n", t.text, n.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::PathExists(path, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    bool t{id} = ae_path_exists({}.ptr, {line}, {col});\n", p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::PathIsDir(path, line, col) => {
            let p = emit_expr(path, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    bool t{id} = ae_path_is_dir({}.ptr, {line}, {col});\n", p.text));
            if p.owned { drop_owned_temp(out, &p, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::ParseInt(text, ty, line, col) => {
            let t = emit_expr(text, out, next, false, ctx);
            let id = *next; *next += 1;
            out.push_str(&format!("    {}t{id} = ae_parse_int(AE_VIEW({}), {line}, {col});\n", ty.c_temp(ctx), t.text));
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: *ty, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::ToString(value, line, col) => {
            let t = emit_expr(value, out, next, false, ctx);
            let id = *next; *next += 1;
            if matches!(t.ty, Type::Int(IntTy::U64 | IntTy::Usize)) {
                out.push_str(&format!("    ae_string t{id} = ae_to_string_u64((uint64_t)({}), {line}, {col});\n", t.text));
            } else if t.ty == Type::Char {
                out.push_str(&format!("    ae_string t{id} = ae_to_string_char({}, {line}, {col});\n", t.text));
            } else {
                out.push_str(&format!("    ae_string t{id} = ae_to_string({}, {line}, {col});\n", t.text));
            }
            if t.owned { drop_owned_temp(out, &t, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        // A closure literal bound to a name emits nothing: the declaration
        // statement skips its store (ADR-0188). Where a function value is
        // expected, the emitted text is the generated function itself, whose
        // address is the C function pointer (ADR-0189).
        Expr::ClosureLit(cid) => {
            let fn_id = ctx.closure_fn_id(*cid);
            Emitted { text: format!("ae_fn_{fn_id}"), ty: Type::Closure(*cid), owned: false, moves: Vec::new(), materialized: false }
        }
        // ADR-0214 / spec/concurrency.md: the captures move into a heap
        // context whose first member is the return slot, so wait reads it
        // without knowing the capture layout. The runtime's spawn/join carry
        // the platform thread through one spelling; a NULL native handle
        // means the spawn machinery ran out of memory, fatal like every
        // other allocation failure.
        Expr::Spawn(slots, callable, tid, line, column) => {
            let (entry, ctx_ty, fn_member, borrow_captures) = match &**callable {
                Expr::ClosureLit(cid) | Expr::Variable(_, Type::Closure(cid), ..) => {
                    let fn_id = ctx.closure_fn_id(*cid);
                    let borrows = ctx.closure_borrow_captures(*cid);
                    (format!("ae_thread_entry_{fn_id}"), format!("ae_thread_ctx_{fn_id}"), None, borrows)
                }
                // The function value rides the context: the trampoline calls
                // through the packaged pointer.
                Expr::Variable(_, Type::Fn(sig), ..) =>
                    (format!("ae_thread_entry_f{sig}"), format!("ae_thread_fncall_{sig}"), Some(callable.clone()), Vec::new()),
                other => unreachable!("spawn callable shape: {other:?}"),
            };
            let t = *next; *next += 1;
            out.push_str(&format!("    {ctx_ty} *ctx{t} = ae_alloc(sizeof({ctx_ty}), {line}, {column});\n"));
            let mut moves = Vec::new();
            for (index, slot) in slots.iter().enumerate() {
                let value = emit_expr(slot, out, next, true, ctx);
                moves.extend(value.moves);
                out.push_str(&format!("    ctx{t}->f{index} = {};\n", value.text));
            }
            // The borrow captures read the caller's storage through the
            // read-only forms the closure's own parameters carry; the join
            // proof (ADR-0215) guarantees the storage outlives the reads.
            let value_base = slots.len();
            for (index, (outer_id, hidden)) in borrow_captures.iter().enumerate() {
                let field = value_base + index;
                let form = match hidden {
                    Type::StrView => format!("AE_VIEW(v{outer_id})"),
                    Type::VectorRef(_) | Type::StructRef(_) => format!("&v{outer_id}"),
                    other => unreachable!("borrow capture form: {other:?}"),
                };
                out.push_str(&format!("    ctx{t}->f{field} = {};
", form));
            }
            if let Some(fn_value) = fn_member {
                let value = emit_expr(&fn_value, out, next, false, ctx);
                moves.extend(value.moves);
                out.push_str(&format!("    ctx{t}->fn = {};\n", value.text));
            }
            out.push_str(&format!("    void *native{t} = ae_thread_spawn({entry}, ctx{t});\n"));
            out.push_str(&format!("    if (!native{t}) ae_fail(\"out of memory\", {line}, {column});\n"));
            null_moves(out, &moves, ctx);
            out.push_str(&format!("    ae_thread_slot *t{t} = ae_alloc(sizeof(ae_thread_slot), {line}, {column});\n"));
            out.push_str(&format!("    t{t}->native = native{t};\n"));
            out.push_str(&format!("    t{t}->ctx = (void *)ctx{t};\n"));
            Emitted { text: format!("t{t}"), ty: Type::Thread(*tid), owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Wait(handle, ret, line, column) => {
            // The handle moves into wait: the slot and its context are
            // released here, and the return value — the context's first
            // member — transfers to the temporary. A NULL slot is a moved
            // handle, which safe code cannot reach past the move check.
            let h = emit_expr(handle, out, next, true, ctx);
            let t = *next; *next += 1;
            let ret_spelling = ret.c_temp(ctx).trim_end().to_owned();
            out.push_str(&format!("    ae_thread_slot *s{t} = {};\n", h.text));
            out.push_str(&format!("    if (!s{t}) ae_fail(\"use of a moved thread handle\", {line}, {column});\n"));
            out.push_str(&format!("    ae_thread_join(s{t}->native);\n"));
            out.push_str(&format!("    {ret_spelling} t{t} = *({ret_spelling} *)s{t}->ctx;\n"));
            out.push_str(&format!("    ae_free(s{t}->ctx);\n"));
            out.push_str(&format!("    ae_free(s{t});\n"));
            null_moves(out, &h.moves, ctx);
            Emitted { text: format!("t{t}"), ty: *ret, owned: is_owned_type(*ret, ctx.structs, ctx.enums), moves: Vec::new(), materialized: true }
        }
        Expr::Field(base, fname, fty) => {
            let b = emit_expr(base, out, next, false, ctx);
            if is_owned_type(*fty, ctx.structs, ctx.enums) {
                // Owned fields are borrows: inline, consumer never releases.
                Emitted { text: format!("{}.{}", b.text, fname), ty: *fty, owned: false, moves: b.moves, materialized: true }
            } else {
                // Scalars copy immediately: a later release of the base must
                // not clobber the value (return paths release before return).
                Emitted { text: format!("{}.{}", b.text, fname), ty: *fty, owned: false, moves: b.moves, materialized: false }
            }
        }
        Expr::StrEq(a, b, negated) => {
            let l = emit_expr(a, out, next, false, ctx);
            let r = emit_expr(b, out, next, false, ctx);
            let id = *next; *next += 1;
            let cmp = format!("ae_str_eq(AE_VIEW({}), AE_VIEW({}))", l.text, r.text);
            let text = if *negated { format!("!{cmp}") } else { cmp };
            out.push_str(&format!("    bool t{id} = {text};
"));
            if l.owned { drop_owned_temp(out, &l, ctx); }
            if r.owned { drop_owned_temp(out, &r, ctx); }
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Binary(a, op, b, line, column, ty) if *op == Kind::And || *op == Kind::Or => {
            let l = emit_expr(a, out, next, false, ctx);
            let id = *next; *next += 1;
            // `&&` starts false and fills the result only when the left side
            // is true; `||` mirrors it. The right operand's statements land
            // inside the if body, giving real short-circuit semantics.
            let initial = if *op == Kind::And { "false" } else { "true" };
            let guard = if *op == Kind::And { l.text.clone() } else { format!("!{}", l.text) };
            out.push_str(&format!("    bool t{id} = {initial};\n    if ({guard}) {{\n"));
            let r = emit_expr(b, out, next, false, ctx);
            out.push_str(&format!("    t{id} = {};\n    }}\n", r.text));
            Emitted { text: format!("t{id}"), ty: Type::Bool, owned: false, moves: Vec::new(), materialized: true }
        }
        Expr::Binary(a, op, b, line, column, ty) if *op == Kind::Plus && *ty == Type::String => {
            // A literal operand is program text, not an owned value: emitting
            // it as ae_str_from would allocate and free a fresh buffer on
            // every evaluation only so ae_str_cat could free it again.
            // The fused helpers copy the literal straight into the result and
            // consume just the non-literal side. A literal is a pure
            // expression, so skipping its evaluation changes nothing.
            let owned_part = |part: &Emitted| if part.owned {
                part.text.clone()
            } else {
                format!("ae_str_copy_view(AE_VIEW({}), {line}, {column})", part.text)
            };
            let id = *next; *next += 1;
            // A `to_string(i64)` on the other side also skips the allocator:
            // the digits are formatted into a stack buffer that dies with the
            // expression, so `"k" + to_string(n)` performs one allocation in
            // total. Same formatter, same value, no owned temporary.
            let digits_of = |part: &Expr, out: &mut String, next: &mut usize| -> Option<(String, Type)> {
                if let Expr::ToString(inner, _, _) = part
                    && matches!(crate::frontend::mir::expr_type(inner, ctx), Type::Int(_))
                {
                    let v = emit_expr(inner, out, next, false, ctx);
                    return Some((v.text, v.ty));
                }
                None
            };
            if let Expr::StrLit(text, _, _) = a.as_ref() {
                if let Some((value, vty)) = digits_of(b, out, next) {
                    if matches!(vty, Type::Int(IntTy::U64 | IntTy::Usize)) {
                        out.push_str(&format!("    char buf{id}[24];\n    const int64_t dlen{id} = ae_u64_to_buf((uint64_t)({value}), buf{id}, {line}, {column});\n"));
                    } else {
                        out.push_str(&format!("    char buf{id}[24];\n    const int64_t dlen{id} = ae_int_to_buf({value}, buf{id}, {line}, {column});\n"));
                    }
                    out.push_str(&format!("    ae_string t{id} = ae_str_cat_lit_buf(\"{text}\", buf{id}, dlen{id}, {line}, {column});\n"));
                    return Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true };
                }
                let r = emit_expr(b, out, next, true, ctx);
                let right = owned_part(&r);
                out.push_str(&format!("    ae_string t{id} = ae_str_cat_lit_left(\"{text}\", {right}, {line}, {column});\n"));
                null_moves(out, &r.moves, ctx);
                return Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true };
            }
            if let Expr::StrLit(text, _, _) = b.as_ref() {
                if let Some((value, vty)) = digits_of(a, out, next) {
                    if matches!(vty, Type::Int(IntTy::U64 | IntTy::Usize)) {
                        out.push_str(&format!("    char buf{id}[24];\n    const int64_t dlen{id} = ae_u64_to_buf((uint64_t)({value}), buf{id}, {line}, {column});\n"));
                    } else {
                        out.push_str(&format!("    char buf{id}[24];\n    const int64_t dlen{id} = ae_int_to_buf({value}, buf{id}, {line}, {column});\n"));
                    }
                    out.push_str(&format!("    ae_string t{id} = ae_str_cat_buf_lit(buf{id}, dlen{id}, \"{text}\", {line}, {column});\n"));
                    return Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true };
                }
                let l = emit_expr(a, out, next, true, ctx);
                let left = owned_part(&l);
                out.push_str(&format!("    ae_string t{id} = ae_str_cat_lit_right({left}, \"{text}\", {line}, {column});\n"));
                null_moves(out, &l.moves, ctx);
                return Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true };
            }
            let l = emit_expr(a, out, next, true, ctx);
            let r = emit_expr(b, out, next, true, ctx);
            // ae_str_cat frees both operands, so only a value this
            // expression actually owns may be handed over. Views, vec elements
            // and struct fields are borrows: they are copied instead, and the
            // borrow stays valid and untouched.
            let (left, right) = (owned_part(&l), owned_part(&r));
            out.push_str(&format!("    ae_string t{id} = ae_str_cat({left}, {right}, {line}, {column});\n"));
            let mut moves = l.moves; moves.extend(r.moves);
            null_moves(out, &moves, ctx);
            Emitted { text: format!("t{id}"), ty: Type::String, owned: true, moves: Vec::new(), materialized: true }
        }
        Expr::Binary(a, op, b, line, column, _) => {
            let l = emit_expr(a, out, next, false, ctx); let r = emit_expr(b, out, next, false, ctx);
            // f64 arithmetic maps to the C operators: no overflow trapping, no
            // fast-math, and division goes through a checked helper so `x / 0.0`
            // is a reported failure instead of a silent infinity (ADR-37).
            let floating_f64 = l.ty == Type::Float(FloatTy::F64);
            let floating_f32 = l.ty == Type::Float(FloatTy::F32);
            let floating = floating_f64 || floating_f32;
            let suffix = match l.ty {
                Type::Int(IntTy::I64) => String::new(),
                Type::Int(width) => format!("_{}", width.name()),
                _ => String::new(),
            };
            let text = match op {
                Kind::EqEq => format!("{} == {}", l.text, r.text),
                Kind::NotEq => format!("{} != {}", l.text, r.text),
                Kind::Less => format!("{} < {}", l.text, r.text),
                Kind::LessEq => format!("{} <= {}", l.text, r.text),
                Kind::Greater => format!("{} > {}", l.text, r.text),
                Kind::GreaterEq => format!("{} >= {}", l.text, r.text),
                Kind::Plus if floating => format!("{} + {}", l.text, r.text),
                Kind::Minus if floating => format!("{} - {}", l.text, r.text),
                Kind::Star if floating => format!("{} * {}", l.text, r.text),
                Kind::Slash if floating_f32 => format!("ae_f32_div({}, {}, {line}, {column})", l.text, r.text),
                Kind::Slash if floating_f64 => format!("ae_f64_div({}, {}, {line}, {column})", l.text, r.text),
                Kind::Plus => format!("ae_add{suffix}({}, {}, {line}, {column})", l.text, r.text),
                Kind::Minus => format!("ae_sub{suffix}({}, {}, {line}, {column})", l.text, r.text),
                Kind::Star => format!("ae_mul{suffix}({}, {}, {line}, {column})", l.text, r.text),
                Kind::Slash => format!("ae_div{suffix}({}, {}, {line}, {column})", l.text, r.text),
                Kind::Percent => format!("ae_mod{suffix}({}, {}, {line}, {column})", l.text, r.text),
                Kind::Pipe => match l.ty {
                    Type::Int(width) if width != IntTy::I64 => format!("({})({} | {})", width.c_name(), l.text, r.text),
                    _ => format!("({} | {})", l.text, r.text),
                },
                Kind::Caret => match l.ty {
                    Type::Int(width) if width != IntTy::I64 => format!("({})({} ^ {})", width.c_name(), l.text, r.text),
                    _ => format!("({} ^ {})", l.text, r.text),
                },
                Kind::Amp => match l.ty {
                    Type::Int(width) if width != IntTy::I64 => format!("({})({} & {})", width.c_name(), l.text, r.text),
                    _ => format!("({} & {})", l.text, r.text),
                },
                Kind::Shl => match l.ty {
                    Type::Int(IntTy::I64) => format!("((int64_t)((uint64_t)({}) << (({}) & 63)))", l.text, r.text),
                    Type::Int(_) => format!("ae_shl{suffix}({}, {}, {line}, {column})", l.text, r.text),
                    _ => unreachable!(),
                },
                Kind::Shr => match l.ty {
                    Type::Int(IntTy::I64) => format!("({} >> (({}) & 63))", l.text, r.text),
                    Type::Int(_) => format!("ae_shr{suffix}({}, {}, {line}, {column})", l.text, r.text),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
            let ty = if matches!(op, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq) {
                Type::Bool
            } else {
                l.ty
            };
            Emitted { text, ty, owned: false, moves: Vec::new(), materialized: false }
        }
    };
    if emitted.materialized { return emitted; }
    let id = *next; *next += 1;
    out.push_str(&format!("    {}t{id} = {};\n", emitted.ty.c_temp(ctx), emitted.text));
    // Materializing keeps the expression's ownership: a view of an owned
    // element (`v[i]` of a `vec[vec]`) is a *borrow* even though its storage
    // type is owned, so the temp must never be released (ADR-0235 slice 4).
    // A fresh value already reports `owned: true` and is released here.
    Emitted { text: format!("t{id}"), ..emitted }
}
fn slice_type(ctx: &CompileContext, elem: Type) -> Type {
    Type::Slice(ctx.find_slice_elem(elem).expect("pre-interned slice element type"))
}
fn vector_type(ctx: &CompileContext, elem: Type) -> Type {
    Type::Vector(ctx.find_vector_elem(elem).expect("pre-interned vector element type"))
}

pub(super) fn release_var(out: &mut String, id: usize, ty: Type, ctx: &CompileContext) {
    match ty {
        Type::Vector(elem_id) => out.push_str(&format!("    {}_release(&v{id});\n", c_output::vector_family(ctx, elem_id))),
        Type::MapOf(map_id) => out.push_str(&format!("    {}_release(&v{id});\n", c_output::map_family(ctx, map_id))),
        Type::Struct(k) => out.push_str(&format!("    ae_t{k}_release(&v{id});\n")),
        Type::Enum(_, k) => out.push_str(&format!("    ae_e{k}_release(&v{id});\n")),
        // A never-joined handle leaks its thread slot (spec/concurrency.md
        // §1 rule 3); the release is idempotent because wait nulls the
        // variable first.
        Type::Thread(_) => out.push_str(&format!("    ae_free(v{id});\n")),
        // The cell/box/mutex is process-lifetime (spec/concurrency.md §8–§9):
        // nothing releases it, and copies share it.
        Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan => {}
        // ADR-0216: scope exit unlocks the mutex guard (idempotent if moved).
        Type::Guard => out.push_str(&format!("    ae_mutex_unlock(&v{id});\n")),
        Type::TaskScope => out.push_str(&format!("    ae_scope_release(&v{id});\n")),
        // A view borrows another owner's bytes and releases nothing
        // (ADR-0245 stage 2: a heap view binds like any other view).
        Type::StrView => {}
        Type::VectorRef(_) => {}
        Type::MapRef(_) => {}
        _ => out.push_str(&format!("    ae_str_release(&v{id});\n")),
    }
}
// Release one enum payload or struct field in place, addressed by an arbitrary
// C lvalue. Propagation discards Ok/Some payloads it does not bind, and those
// payloads may be any owned type, not just strings (ADR-34).
pub(super) fn release_member(out: &mut String, member: &str, ty: Type, ctx: &CompileContext) {
    match ty {
        Type::Vector(elem_id) => out.push_str(&format!("    {}_release(&{member});\n", c_output::vector_family(ctx, elem_id))),
        Type::MapOf(map_id) => out.push_str(&format!("    {}_release(&{member});\n", c_output::map_family(ctx, map_id))),
        Type::Struct(k) => out.push_str(&format!("    ae_t{k}_release(&{member});\n")),
        Type::Enum(_, k) => out.push_str(&format!("    ae_e{k}_release(&{member});\n")),
        // A view member releases nothing (it borrows another owner's bytes).
        Type::StrView => {}
        Type::VectorRef(_) | Type::MapRef(_) => {}
        _ => out.push_str(&format!("    ae_str_release(&{member});\n")),
    }
}
// An owned temporary (not a named slot) that a statement has finished using.
pub(super) fn drop_owned_temp(out: &mut String, emitted: &Emitted, ctx: &CompileContext) {
    match emitted.ty {
        Type::Vector(elem_id) => out.push_str(&format!("    {}_release(&{});\n", c_output::vector_family(ctx, elem_id), emitted.text)),
        Type::String => out.push_str(&format!("    ae_str_release(&{});\n", emitted.text)),
        Type::MapOf(map_id) => out.push_str(&format!("    {}_release(&{});\n", c_output::map_family(ctx, map_id), emitted.text)),
        Type::Struct(k) => out.push_str(&format!("    ae_t{k}_release(&{});\n", emitted.text)),
        Type::Enum(_, k) => out.push_str(&format!("    ae_e{k}_release(&{});\n", emitted.text)),
        Type::Guard => out.push_str(&format!("    ae_mutex_unlock(&{});\n", emitted.text)),
        Type::TaskScope => out.push_str(&format!("    ae_scope_release(&{});\n", emitted.text)),
        _ => out.push_str(&format!("    free({});\n", emitted.text)),
    }
}
pub(super) fn null_moves(out: &mut String, moves: &[(usize, Type)], ctx: &CompileContext) {
    for &(id, ty) in moves {
        match ty {
            Type::Vector(elem_id) => out.push_str(&format!("    {}_forfeit(&v{id});\n", c_output::vector_family(ctx, elem_id))),
            Type::String => out.push_str(&format!("    ae_str_forfeit(&v{id});\n")),
            Type::MapOf(map_id) => out.push_str(&format!("    {}_forfeit(&v{id});\n", c_output::map_family(ctx, map_id))),
            Type::Struct(k) => out.push_str(&format!("    ae_t{k}_forfeit(&v{id});\n")),
            Type::Enum(_, k) => out.push_str(&format!("    ae_e{k}_forfeit(&v{id});\n")),
            Type::Guard => out.push_str(&format!("    v{id}.lock = NULL;\n")),
            Type::TaskScope => out.push_str(&format!("    v{id}.state = NULL;\n")),
            _ => out.push_str(&format!("    v{id} = NULL;\n")),
        }
    }
}
pub(super) fn emit_print(printed: &Printed, out: &mut String, next: &mut usize, ctx: &CodegenCtx<'_>) {
    match printed {
        Printed::Expr(e) => {
            let t = emit_expr(e, out, next, false, ctx);
            match t.ty {
                Type::String | Type::StrView => {
                    let pv = *next; *next += 1;
                    out.push_str(&format!("    ae_strview t{pv} = AE_VIEW({});\n", t.text));
                    out.push_str(&format!("    ae_print_view(t{pv});\n"));
                    if t.owned { drop_owned_temp(out, &t, ctx); }
                }
                Type::Bool => out.push_str(&format!("    ae_print_bool({});\n", t.text)),
                Type::Float(FloatTy::F64) => out.push_str(&format!("    ae_f64_print({});\n", t.text)),
                Type::Float(FloatTy::F32) => out.push_str(&format!("    ae_f64_print((double)({}));\n", t.text)),
                Type::Char => out.push_str(&format!("    ae_print_char({});\n", t.text)),
                Type::Int(IntTy::U64 | IntTy::Usize) => out.push_str(&format!("    ae_print_u64((uint64_t)({}));\n", t.text)),
                _ => out.push_str(&format!("    ae_print_i64({});\n", t.text)),
            }
        }
        Printed::Text(text) => {
            // The literal body is already a valid C string body (the lexer
            // checked the escapes), so it is passed as a plain C string: one copy
            // in the generated C, no format string to misinterpret.
            out.push_str(&format!("    ae_print_cstr(\"{text}\");\n"));
        }
    }
}
fn emit_propagation_payload(out: &mut String, subject: &str, layout: usize, fields: &[Type], binding: Option<(usize, Type)>, ctx: &CodegenCtx<'_>) {
    if let Some((id, ty)) = binding {
        out.push_str(&format!("    {}v{id} = {subject}.u.v0.p0;\n", ty.c_temp(ctx)));
    } else {
        for (index, field) in fields.iter().enumerate() {
            if is_owned_type(*field, ctx.structs, ctx.enums) {
                release_member(out, &format!("{subject}.u.v0.p{index}"), *field, ctx);
            }
        }
    }
    if !ctx.enums[layout].is_copy {
        out.push_str(&format!("    ae_e{layout}_forfeit(&{subject});\n"));
    }
}

pub(super) fn emit_value_before_cleanup(
    expr: &Expr,
    frees: &[(usize, Type)],
    out: &mut String,
    next: &mut usize,
    moving: bool, ctx: &CodegenCtx<'_>,
) -> String {
    let value = emit_expr(expr, out, next, moving, ctx);
    // An inline value can still refer to storage a destructor can modify.
    let result = if value.ty == Type::Unit {
        String::new()
    } else if frees.is_empty() {
        value.text
    } else {
        let id = *next;
        *next += 1;
        out.push_str(&format!("    {}t{id} = {};\n", value.ty.c_temp(ctx), value.text));
        format!("t{id}")
    };
    for &(id, ty) in frees {
        if !value.moves.iter().any(|&(moved, _)| moved == id) {
            release_var(out, id, ty, ctx);
        }
    }
    null_moves(out, &value.moves, ctx);
    result
}

pub(super) fn emit_statements(stmts: &[Statement], out: &mut String, next: &mut usize, ctx: &CodegenCtx<'_>) {
    for stmt in stmts {
        match stmt {
            Statement::Return(expr, frees) => {
                let result = emit_value_before_cleanup(expr, frees, out, next, true, ctx);
                if result.is_empty() {
                    out.push_str("    return;\n");
                } else {
                    out.push_str(&format!("    return {result};\n"));
                }
            }
            Statement::Declare(id, e) => {
                // A closure binding names its generated function and stores
                // nothing, so the declaration emits no C at all (ADR-0188).
                if matches!(e, Expr::ClosureLit(_)) { }
                else {
                    let t = emit_expr(e, out, next, true, ctx);
                    if t.ty != Type::Unit {
                        out.push_str(&format!("    {}v{id} = {};\n", t.ty.c_temp(ctx), t.text));
                    }
                    null_moves(out, &t.moves, ctx);
                }
            }
            Statement::Array(id, values, elem_ty) => {
                let items: Vec<_> = values.iter().map(|e| emit_expr(e, out, next, false, ctx).text).collect();
                out.push_str(&format!("    {} v{id}[{}] = {{", c_output::c_type_field(*elem_ty, ctx), items.len()));
                for item in items { out.push_str(&format!("{item},")); }
                out.push_str("};\n");
            }
            Statement::Store(id, len, index, value, line, col, _) => {
                // The same constant-index proof the read uses (M2.10):
                // in range, the check elides; out of range, it stays and
                // traps at the same site.
                let constant_in_range = matches!(index, Expr::Number(n) if *n >= 0 && (*n as usize) < *len);
                let i = emit_expr(index, out, next, false, ctx);
                let slot = if constant_in_range {
                    match index { Expr::Number(n) => format!("{n}"), _ => unreachable!() }
                } else {
                    let checked = *next; *next += 1;
                    out.push_str(&format!("    size_t t{checked} = ae_index({}, {len}, {line}, {col});\n", i.text));
                    format!("t{checked}")
                };
                let value = emit_expr(value, out, next, false, ctx);
                out.push_str(&format!("    v{id}[{slot}] = {};\n", value.text));
            }
            Statement::Sort(id, strings, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                // The string family keeps its own byte-order sort; the i64
                // vector is an `ae_gvec` and sorts in place through qsort.
                let callee = if *strings { "ae_strvec_sort" } else { "ae_gvec_sort_i64" };
                out.push_str(&format!("    {callee}(&v{id});\n"));
            }
            Statement::Reverse(id, strings, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                let callee = if *strings { "ae_strvec_reverse" } else { "ae_gvec_reverse_i64" };
                out.push_str(&format!("    {callee}(&v{id});\n"));
            }
            Statement::GVecPush(id, value, elem_ty, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                // An owned element moves into the vector: the source slot is
                // forfeited after the bytes are copied (ADR-0235 slice 4).
                let moving = is_owned_type(*elem_ty, ctx.structs, ctx.enums);
                let value = emit_expr(value, out, next, moving, ctx);
                let tmp = *next; *next += 1;
                let c_elem = c_output::c_type_field(*elem_ty, ctx);
                out.push_str(&format!("    {c_elem} t{tmp} = {};\n", value.text));
                out.push_str(&format!("    ae_gvec_push(&v{id}, &t{tmp}, {line}, {col});\n"));
                null_moves(out, &value.moves, ctx);
            }
            Statement::GVecPop(id, _, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                out.push_str(&format!("    ae_gvec_pop(&v{id}, {line}, {col});\n"));
            }
            Statement::SliceStore(id, index, value, elem_ty, line, col) => {
                // The index is checked before the right side, like the vector
                // store; `ae_slice_mut_at` hands back the element's address
                // through the exclusive borrow (ADR-0236 rung 4).
                let i = emit_expr(index, out, next, false, ctx);
                let idx_tmp = *next; *next += 1;
                out.push_str(&format!("    int64_t t{idx_tmp} = ae_vec_index({}, v{id}.len, {line}, {col});\n", i.text));
                let value = emit_expr(value, out, next, false, ctx);
                let val_tmp = *next; *next += 1;
                let c_elem = c_output::c_type_field(*elem_ty, ctx);
                out.push_str(&format!("    {c_elem} t{val_tmp} = {};\n", value.text));
                out.push_str(&format!("    *(({c_elem} *)ae_slice_mut_at(v{id}, t{idx_tmp}, {line}, {col})) = t{val_tmp};\n"));
                null_moves(out, &value.moves, ctx);
            }
            Statement::GVecStore(id, index, value, elem_ty, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                // The index is computed and checked before the right side (the
                // store's own check in ae_gvec_set is a backstop only).
                let i = emit_expr(index, out, next, false, ctx);
                let idx_tmp = *next; *next += 1;
                out.push_str(&format!("    int64_t t{idx_tmp} = ae_vec_index({}, v{id}.len, {line}, {col});\n", i.text));
                let moving = is_owned_type(*elem_ty, ctx.structs, ctx.enums);
                let value = emit_expr(value, out, next, moving, ctx);
                let val_tmp = *next; *next += 1;
                let c_elem = c_output::c_type_field(*elem_ty, ctx);
                out.push_str(&format!("    {c_elem} t{val_tmp} = {};\n", value.text));
                out.push_str(&format!("    ae_gvec_set(&v{id}, t{idx_tmp}, &t{val_tmp}, {line}, {col});\n"));
                null_moves(out, &value.moves, ctx);
            }
            // P2.4: vector operations on a struct's vector field (`r.cells`).
            // The receiver is the field lvalue; a `&mut` root reads through
            // its pointer. Each arm mirrors its slot form exactly.
            Statement::VecPushField(id, field, base_ty, value, elem_ty, str_view, line, col) => {
                let member = field_member(*id, field, *base_ty);
                out.push_str(&format!("    if (!({member}).data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                match elem_ty {
                    Type::String => {
                        let borrowed = *str_view || string_expr_is_view(value);
                        let value = emit_expr(value, out, next, !borrowed, ctx);
                        let text = if borrowed { format!("ae_str_copy_view(AE_VIEW({}), {line}, {col})", value.text) } else { value.text.clone() };
                        out.push_str(&format!("    ae_strvec_push(&({member}), {text}, {line}, {col});\n"));
                        null_moves(out, &value.moves, ctx);
                    }
                    _ => {
                        let moving = is_owned_type(*elem_ty, ctx.structs, ctx.enums);
                        let value = emit_expr(value, out, next, moving, ctx);
                        let tmp = *next; *next += 1;
                        let c_elem = c_output::c_type_field(*elem_ty, ctx);
                        out.push_str(&format!("    {c_elem} t{tmp} = {};\n", value.text));
                        out.push_str(&format!("    ae_gvec_push(&({member}), &t{tmp}, {line}, {col});\n"));
                        null_moves(out, &value.moves, ctx);
                    }
                }
            }
            Statement::VecPopField(id, field, base_ty, elem_ty, line, col) => {
                let member = field_member(*id, field, *base_ty);
                out.push_str(&format!("    if (!({member}).data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                let pop = if *elem_ty == Type::String { "ae_strvec_pop" } else { "ae_gvec_pop" };
                out.push_str(&format!("    {pop}(&({member}), {line}, {col});\n"));
            }
            Statement::VecStoreField(id, field, base_ty, index, value, elem_ty, str_view, line, col) => {
                let member = field_member(*id, field, *base_ty);
                out.push_str(&format!("    if (!({member}).data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                let i = emit_expr(index, out, next, false, ctx);
                let checked = *next; *next += 1;
                out.push_str(&format!("    int64_t t{checked} = ae_vec_index({}, ({member}).len, {line}, {col});\n", i.text));
                match elem_ty {
                    Type::String => {
                        let borrowed = *str_view || string_expr_is_view(value);
                        let value = emit_expr(value, out, next, !borrowed, ctx);
                        let text = if borrowed { format!("ae_str_copy_view(AE_VIEW({}), {line}, {col})", value.text) } else { value.text.clone() };
                        let replacement = *next; *next += 1;
                        out.push_str(&format!("    {}t{replacement} = {text};\n", Type::String.c_temp(ctx)));
                        out.push_str(&format!("    ae_strvec_clear_slot(&({member}), t{checked}, {line}, {col});\n"));
                        out.push_str(&format!("    ({member}).data[t{checked}] = t{replacement};\n"));
                        null_moves(out, &value.moves, ctx);
                    }
                    _ => {
                        let moving = is_owned_type(*elem_ty, ctx.structs, ctx.enums);
                        let value = emit_expr(value, out, next, moving, ctx);
                        let val_tmp = *next; *next += 1;
                        let c_elem = c_output::c_type_field(*elem_ty, ctx);
                        out.push_str(&format!("    {c_elem} t{val_tmp} = {};\n", value.text));
                        out.push_str(&format!("    ae_gvec_set(&({member}), t{checked}, &t{val_tmp}, {line}, {col});\n"));
                        null_moves(out, &value.moves, ctx);
                    }
                }
            }
            Statement::VecSortField(id, field, base_ty, elem_ty, reverse, line, col) => {
                let member = field_member(*id, field, *base_ty);
                out.push_str(&format!("    if (!({member}).data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                let callee = match elem_ty {
                    Type::String => if *reverse { "ae_strvec_reverse" } else { "ae_strvec_sort" },
                    _ => if *reverse { "ae_gvec_reverse_i64" } else { "ae_gvec_sort_i64" },
                };
                out.push_str(&format!("    {callee}(&({member}));\n"));
            }
            Statement::StrVecPush(id, value, line, col, borrowed) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                // A borrowed element (view binding, string field, string
                // vector element) is copied into the vector; only an owned
                // string is moved, because moving a view would empty the slot
                // it was read from.
                let borrowed = *borrowed || string_expr_is_view(value);
                let value = emit_expr(value, out, next, !borrowed, ctx);
                let text = if borrowed { format!("ae_str_copy_view(AE_VIEW({}), {line}, {col})", value.text) } else { value.text.clone() };
                out.push_str(&format!("    ae_strvec_push(&v{id}, {text}, {line}, {col});\n"));
                null_moves(out, &value.moves, ctx);
            }
            Statement::StrVecPop(id, line, col) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                out.push_str(&format!("    ae_strvec_pop(&v{id}, {line}, {col});\n"));
            }
            Statement::StrVecStore(id, index, value, line, col, borrowed) => {
                out.push_str(&format!("    if (!v{id}.data) ae_fail(\"use of moved vector\", {line}, {col});\n"));
                let i = emit_expr(index, out, next, false, ctx);
                let checked = *next; *next += 1;
                out.push_str(&format!("    int64_t t{checked} = ae_strvec_index({}, v{id}.len, {line}, {col});\n", i.text));
                let borrowed = *borrowed || string_expr_is_view(value);
                let value = emit_expr(value, out, next, !borrowed, ctx);
                let text = if borrowed { format!("ae_str_copy_view(AE_VIEW({}), {line}, {col})", value.text) } else { value.text.clone() };
                // The source may borrow the destination element, so copy or
                // evaluate it before releasing that element.
                let replacement = *next; *next += 1;
                out.push_str(&format!("    {}t{replacement} = {text};\n", Type::String.c_temp(ctx)));
                out.push_str(&format!("    ae_strvec_clear_slot(&v{id}, t{checked}, {line}, {col});\n"));
                out.push_str(&format!("    v{id}.data[t{checked}] = t{replacement};\n"));
                null_moves(out, &value.moves, ctx);
            }
            Statement::FieldStore(id, path, fty, value, base_ty) => {
                if *fty == Type::Unit {
                    // A unit field has no storage: run the right side for side
                    // effects, then write nothing (`t = ;` would be invalid C).
                    let value = emit_expr(value, out, next, true, ctx);
                    null_moves(out, &value.moves, ctx);
                } else {
                // Overwriting an owned field releases the old value first.
                // A mutable borrow writes through a pointer, so
                // every access reads `v{id}->field` instead of `v{id}.field`
                // (ADR-0185); the release targets what the caller owns.
                let member = field_member(*id, path, *base_ty);
                let release_old = match fty {
                    Type::String => format!("    ae_str_release(&{member});\n"),
                    Type::Vector(elem_id) => format!("    {}_release(&{member});\n", c_output::vector_family(ctx, *elem_id)),
                    Type::MapOf(map_id) => format!("    {}_release(&{member});\n", c_output::map_family(ctx, *map_id)),
                    Type::Struct(k) if !ctx.structs[*k].is_copy => format!("    ae_t{k}_release(&{member});\n"),
                    Type::Enum(_, k) if !ctx.enums[*k].is_copy => format!("    ae_e{k}_release(&{member});\n"),
                    _ => String::new(),
                };
                // The value emits in a moving context: an owned variable
                // on the right side is consumed and zeroed here, so the
                // field owns the bytes and the source does not (no
                // aliasing, no double release at the scope ends).
                let value = emit_expr(value, out, next, true, ctx);
                // Snapshot the result before releasing the old field: even a
                // non-materialized expression can read that same field.
                let temp = *next; *next += 1;
                out.push_str(&format!("    {}t{temp} = {};\n", fty.c_temp(ctx), value.text));
                out.push_str(&release_old);
                out.push_str(&format!("    {member} = t{temp};\n"));
                null_moves(out, &value.moves, ctx);
                }
            }
            Statement::Assign(id, e, release_old) => {
                let t = emit_expr(e, out, next, true, ctx);
                if *release_old { release_var(out, *id, t.ty, ctx); }
                if t.ty != Type::Unit {
                    out.push_str(&format!("    v{id} = {};\n", t.text));
                }
                null_moves(out, &t.moves, ctx);
            }
            Statement::Print(printed) => emit_print(printed, out, next, ctx),
            Statement::Exit(code, frees) => {
                let result = emit_value_before_cleanup(code, frees, out, next, false, ctx);
                out.push_str(&format!("    ae_exit({result});\n"));
            }
            Statement::Assert(condition, line, col) => {
                let t = emit_expr(condition, out, next, false, ctx);
                out.push_str(&format!("    if (!({})) ae_fail(\"assertion failed\", {line}, {col});\n", t.text));
            }
            Statement::If(condition, yes, no) => {
                let cond = emit_expr(condition, out, next, false, ctx);
                out.push_str(&format!("    if ({}) {{\n", cond.text)); emit_statements(yes, out, next, ctx);
                out.push_str("    } else {\n"); emit_statements(no, out, next, ctx); out.push_str("    }\n");
            }
            Statement::While(condition, body) => {
                out.push_str("    while (1) {\n");
                let cond = emit_expr(condition, out, next, false, ctx);
                out.push_str(&format!("    if (!({})) break;\n", cond.text));
                emit_statements(body, out, next, ctx); out.push_str("    }\n");
            }
            // The while(1) form re-evaluates the condition after `continue`.
            Statement::Break => out.push_str("    break;\n"),
            Statement::Continue(label) => match label {
                // In a for loop, continue must still run the increment.
                Some(label) => out.push_str(&format!("    goto ae_next_{label};\n")),
                None => out.push_str("    continue;\n"),
            },
            // `m[key] = value;` copies the key into the table, so the key
            // expression may be a temporary or a view.
            Statement::MapPut(place, key, value, line, col) => {
                let k = emit_expr(key, out, next, false, ctx);
                let v = emit_expr(value, out, next, false, ctx);
                let put_fn = if place.f64_value() { "ae_mapf_put" } else { "ae_map_put" };
                out.push_str(&format!("    {put_fn}(&{}, AE_VIEW({}), {}, {line}, {col});\n", place.text(), k.text, v.text));
                null_moves(out, &k.moves, ctx);
                null_moves(out, &v.moves, ctx);
                if k.owned { drop_owned_temp(out, &k, ctx); }
            }
            // Propagating a result returns the error value itself after every
            // live owned value is released, so the failure path owns nothing
            // the caller could reach. The Ok payload moves out of the enum and
            // the enum is then forfeited, which is what keeps it from being
            // freed twice.
            Statement::Propagate(value, ty, fields, binding, frees) => {
                let t = emit_expr(value, out, next, true, ctx);
                let eid = match ty { Type::Enum(_, k) => *k, _ => unreachable!("propagation is enum-typed") };
                null_moves(out, &t.moves, ctx);
                out.push_str(&format!("    if ({}.tag == AE_E{eid}_1) {{\n", t.text));
                for &(id, vty) in frees {
                    if t.moves.iter().any(|&(m, _)| m == id) { continue; }
                    release_var(out, id, vty, ctx);
                }
                out.push_str(&format!("        return {};\n    }}\n", t.text));
                emit_propagation_payload(out, &t.text, eid, fields, *binding, ctx);
            }
            // Arms become an if/else chain rather than a C switch: a `break`
            // in an arm body must leave the enclosing loop, which a switch
            // would swallow. Coverage is exhaustive, so no fall-through case
            // is needed.
            Statement::Match(subject, _, arms) => {
                let mut first = true;
                for arm in arms {
                    let mut conds = Vec::new();
                    collect_pattern_conditions(&arm.pattern, &format!("v{subject}"), &mut conds);
                    if conds.is_empty() {
                        if first {
                            out.push_str("    if (1) {\n");
                        } else {
                            out.push_str("    } else {\n");
                        }
                    } else {
                        let test = conds.join(" && ");
                        if first {
                            out.push_str(&format!("    if ({test}) {{\n"));
                        } else {
                            out.push_str(&format!("    }} else if ({test}) {{\n"));
                        }
                    }
                    first = false;
                    for (bid, pty, path) in &arm.bindings {
                        emit_payload_binding(out, *bid, *subject, path, *pty, ctx);
                    }
                    emit_statements(&arm.body, out, next, ctx);
                }
                if !first {
                    out.push_str("    }\n");
                }
            }
            // CFG-only (M2.3): the match lowering puts a variant's payload
            // bindings at the top of the arm's block. The parser never emits
            // one into the tree, so this arm runs only when the MIR backend
            // emits a match's arm blocks.
            Statement::TakePropagationPayload(subject, ty, fields, binding) => {
                let Type::Enum(_, layout) = *ty else { unreachable!("propagation requires an enum") };
                emit_propagation_payload(out, &format!("v{subject}"), layout, fields, *binding, ctx);
            }
            Statement::BindPayload(bid, subject, path, pty) => {
                emit_payload_binding(out, *bid, *subject, path, *pty, ctx);
            }
            Statement::For(var, end_var, body, label, line, col) => {
                out.push_str("    while (1) {\n");
                out.push_str(&format!("    if (!(v{var} < v{end_var})) break;\n"));
                emit_statements(body, out, next, ctx);
                out.push_str(&format!("    ae_next_{label}: ;\n"));
                out.push_str(&format!("    v{var} = ae_add(v{var}, 1, {line}, {col});\n"));
                out.push_str("    }\n");
            }
            Statement::ResetTokens(cursor, iteration) => {
                out.push_str(&format!("    ae_tokens v{iteration} = v{cursor};\n    ae_tokens_reset(&v{iteration});\n"));
            }
            Statement::AdvanceTokens(_, cursor, elem, advanced) => {
                out.push_str(&format!(
                    "    ae_strview v{elem};\n    bool v{advanced} = ae_tokens_next(&v{cursor}, &v{elem});\n"
                ));
            }
            Statement::ForTokens(cursor, elem, body, label, _line, _col) => {
                // Restart the cursor so a second loop over the same binding
                // behaves like the first; the element is a view into the base,
                // never a copy.
                out.push_str(&format!("    ae_tokens ae_cursor_{elem} = v{cursor};\n    ae_tokens_reset(&ae_cursor_{elem});\n"));
                out.push_str("    while (1) {\n");
                out.push_str(&format!("    ae_strview v{elem};\n"));
                out.push_str(&format!("    if (!ae_tokens_next(&ae_cursor_{elem}, &v{elem})) break;\n"));
                emit_statements(body, out, next, ctx);
                out.push_str(&format!("    ae_next_{label}: ;\n"));
                out.push_str("    }\n");
            }
            Statement::Frees(ids) => { for &(id, ty) in ids { release_var(out, id, ty, ctx); } }
            // M2.12 slice 1: the block's inner statements emit inline; the
            // braces are a scoping container until unchecked operations land.
            Statement::Unsafe(inner) => {
                out.push_str("    {
");
                emit_statements(inner, out, next, ctx);
                out.push_str("    }
");
            }
            // M2.12 slice 3: the write goes through the pointer's address.
            Statement::PtrStore(id, value, _line, _column) => {
                let t = emit_expr(value, out, next, false, ctx);
                out.push_str(&format!("    AE_PTR_WRITE(v{id}, {});
", t.text));
                null_moves(out, &t.moves, ctx);
            }
            // spec/concurrency.md §8: the store is the seq_cst release that
            // a later load in another thread observes.
            Statement::AtomicStore(handle, value) => {
                let a = emit_expr(handle, out, next, false, ctx);
                let v = emit_expr(value, out, next, false, ctx);
                out.push_str(&format!("    ae_cell_store({}, {});
", a.text, v.text));
            }
            Statement::SharedSet(handle, value, sid) => {
                let h = emit_expr(handle, out, next, false, ctx);
                let v = emit_expr(value, out, next, false, ctx);
                let elem_c = c_type_field(ctx.shared_types_get(*sid), ctx);
                out.push_str(&format!("    *({elem_c} *)({}.box) = {};\n", h.text, v.text));
            }
            Statement::ChanClose(handle) => {
                let c = emit_expr(handle, out, next, false, ctx);
                out.push_str(&format!("    ae_chan_close({});\n", c.text));
            }
            Statement::TaskSpawn(ex, slots, _callable, cid, line, column) => {
                let e = emit_expr(ex, out, next, false, ctx);
                let fn_id = ctx.closure_fn_id(*cid);
                let entry = format!("ae_thread_entry_{fn_id}");
                let ctx_ty = format!("ae_thread_ctx_{fn_id}");
                let t = *next; *next += 1;
                out.push_str(&format!("    {ctx_ty} *ctx{t} = ae_alloc(sizeof({ctx_ty}), {line}, {column});\n"));
                let mut moves = Vec::new();
                for (index, slot) in slots.iter().enumerate() {
                    let value = emit_expr(slot, out, next, true, ctx);
                    moves.extend(value.moves);
                    out.push_str(&format!("    ctx{t}->f{index} = {};\n", value.text));
                }
                null_moves(out, &moves, ctx);
                out.push_str(&format!("    ae_task_spawn({}, {entry}, ctx{t});\n", e.text));
            }
            Statement::TaskYield => {
                out.push_str("    ae_task_yield();\n");
            }
            Statement::ScopeSpawn(sc, slots, _callable, cid, line, column) => {
                let s = emit_expr(sc, out, next, false, ctx);
                let fn_id = ctx.closure_fn_id(*cid);
                let entry = format!("ae_thread_entry_{fn_id}");
                let ctx_ty = format!("ae_thread_ctx_{fn_id}");
                let t = *next; *next += 1;
                out.push_str(&format!("    {ctx_ty} *ctx{t} = ae_alloc(sizeof({ctx_ty}), {line}, {column});\n"));
                let mut moves = Vec::new();
                for (index, slot) in slots.iter().enumerate() {
                    let value = emit_expr(slot, out, next, true, ctx);
                    moves.extend(value.moves);
                    out.push_str(&format!("    ctx{t}->f{index} = {};\n", value.text));
                }
                null_moves(out, &moves, ctx);
                out.push_str(&format!("    ae_scope_spawn({}, {entry}, ctx{t});\n", s.text));
            }
            Statement::PoolSpawn(p, slots, _callable, cid, line, column) => {
                let s = emit_expr(p, out, next, false, ctx);
                let fn_id = ctx.closure_fn_id(*cid);
                let entry = format!("ae_thread_entry_{fn_id}");
                let ctx_ty = format!("ae_thread_ctx_{fn_id}");
                let t = *next; *next += 1;
                out.push_str(&format!("    {ctx_ty} *ctx{t} = ae_alloc(sizeof({ctx_ty}), {line}, {column});\n"));
                let mut moves = Vec::new();
                for (index, slot) in slots.iter().enumerate() {
                    let value = emit_expr(slot, out, next, true, ctx);
                    moves.extend(value.moves);
                    out.push_str(&format!("    ctx{t}->f{index} = {};\n", value.text));
                }
                null_moves(out, &moves, ctx);
                out.push_str(&format!("    ae_pool_spawn({}, {entry}, ctx{t});\n", s.text));
            }
        }
    }
}

fn collect_pattern_conditions(pattern: &CompiledPattern, place: &str, out: &mut Vec<String>) {
    match pattern {
        CompiledPattern::Wildcard | CompiledPattern::Binding => {}
        CompiledPattern::Variant { eid, variant, fields } => {
            out.push(format!("{place}.tag == AE_E{eid}_{variant}"));
            for (fidx, _, subpat) in fields {
                let sub_place = format!("{place}.u.v{variant}.p{fidx}");
                collect_pattern_conditions(subpat, &sub_place, out);
            }
        }
    }
}

// One match arm's payload binding, as both backends emit it (M2.3): a
// view/borrow/copy read of the matched variant's payload slot. String
// payloads bind as read-only views, vector payloads as read-only borrows,
// and scalars as plain copies — nothing is ever moved out of a matched enum.
fn emit_payload_binding(
    out: &mut String,
    bid: usize,
    subject: usize,
    path: &[(usize, usize)],
    pty: Type,
    ctx: &CompileContext,
) {
    let mut member = format!("v{subject}");
    for (variant, index) in path {
        member.push_str(&format!(".u.v{variant}.p{index}"));
    }
    match pty {
        Type::String => out.push_str(&format!("    ae_strview v{bid} = AE_VIEW({member});\n")),
        Type::Bool => out.push_str(&format!("    bool v{bid} = {member};\n")),
        Type::Float(width) => out.push_str(&format!("    {} v{bid} = {member};\n", width.c_name())),
        Type::Char => out.push_str(&format!("    uint32_t v{bid} = {member};\n")),
        Type::Vector(elem_id) => out.push_str(&format!("    const {} *v{bid} = &{member};\n", c_output::vector_family(ctx, elem_id))),
        Type::Int(width) => out.push_str(&format!("    {} v{bid} = {member};\n", width.c_name())),
        Type::Fn(sig) => out.push_str(&format!("    ae_fn_t{sig} v{bid} = {member};\n")),
        // A struct payload binds as a read-only view, like a string payload:
        // the origin enum keeps ownership and releases at scope end.
        Type::Struct(k) => out.push_str(&format!("    const ae_t{k} *v{bid} = &{member};\n")),
        Type::Enum(_, k) => out.push_str(&format!("    ae_e{k} v{bid} = {member};\n")),
        _ => out.push_str(&format!("    int64_t v{bid} = {member};\n")),
    }
}
