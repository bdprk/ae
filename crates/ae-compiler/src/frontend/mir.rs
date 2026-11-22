//! The mid-level intermediate representation: places, rvalues, and explicit
//! temporaries over the CFG (M2.2).
//!
//! The CFG (ADR-0191) keeps statements whole. MIR decomposes them: every
//! expression becomes a sequence of `Let`-shaped bindings of temporaries
//! ending in one rvalue, every container update becomes an explicit
//! operation on a place, and every branch keeps the CFG's terminators. This
//! is the shape the optimizers (constant propagation, inlining, bounds-check
//! elimination) and the UB-interpreting interpreter operate on; the C
//! backend continues to consume the statement tree until the MIR backend
//! lands, so this module's contract is "every accepted program lowers, and
//! the printer shows it".
//!
//! Lowering is per statement list: control statements lower into the CFG's
//! block/terminator shape (already computed by `cfg::build_cfg`), and the
//! statements inside each block decompose into bindings.
use super::*;
use super::cfg::{build_cfg, CfgBlock, CfgTerminator};

pub(super) fn mir_path(path: &Path) -> Result<String, String> {
    let program = loader::load_program(path)?;
    let mut out = String::new();
    for file in &program.files {
        for function in file {
            let mut mir = lower_function(function);
            optimize_function(&mut mir);
            // The printed graph is the one the C backend ships, releases
            // included (M2.4), so `emit-mir` shows exactly which drops
            // survived elaboration.
            elaborate_drops(&mut mir);
            validate(&mir)?;
            print_function(&mir, &mut out);
        }
    }
    Ok(out)
}

#[derive(Debug)]
pub(super) struct MirFunction {
    pub(super) id: usize,
    pub(super) is_main: bool,
    pub(super) params: Vec<(usize, Type)>,
    pub(super) returns: Type,
    pub(super) blocks: Vec<MirBlock>,
}

#[derive(Debug)]
pub(super) struct MirBlock {
    pub(super) id: usize,
    // The block's statements, verbatim from the CFG. The bindings above are
    // the decomposed view optimizers and the printer consume; a statement
    // form whose decomposition is not complete yet keeps its whole shape
    // here, so the C backend can emit from this block graph without losing
    // an effect (M2.3).
    pub(super) statements: Vec<Statement>,
    pub(super) bindings: Vec<MirBinding>,
    pub(super) terminator: CfgTerminator,
}

// One decomposition step: a temporary (or a user variable) binds an rvalue.
// The result of the last binding of a statement feeds the statement's own
// effect (a call's result released at the statement's end, a print's
// operand, a store's value).
#[derive(Debug)]
pub(super) struct MirBinding {
    pub(super) target: MirPlace,
    pub(super) rvalue: MirRvalue,
}

#[derive(Debug, Clone)]
pub(super) enum MirPlace {
    Local(usize, Type),
    Field(Box<MirPlace>, String, Type),
    Temp(usize, Type),
}

#[derive(Debug, Clone)]
pub(super) enum MirRvalue {
    Use(MirOperand),
    Binary(MirOperand, Kind, MirOperand, Type),
    ShortCircuit(MirOperand, Kind, Expr),
    Call(usize, Vec<MirOperand>, Type),
    FnCall(usize, Vec<MirOperand>, Type),
    ClosureLit(usize),
    Intrinsic(&'static str, Vec<MirOperand>, Type),
    Aggregate(usize, Vec<MirRvalue>),
    AdtAggregate(&'static str, usize, usize, Vec<MirRvalue>),
    VecNew(Vec<MirRvalue>, Type),
}

#[derive(Debug, Clone)]
pub(super) enum MirOperand {
    Place(MirPlace),
    Borrow { slot: usize, take_address: bool },
    FnPtr(usize),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>, usize, usize),
}

#[derive(Debug)]
pub(super) struct MirLowering {
    next_temp: usize,
}

impl MirLowering {
    fn fresh(&mut self, ty: Type) -> MirPlace {
        let id = self.next_temp;
        self.next_temp += 1;
        MirPlace::Temp(id, ty)
    }
}

// Lower the whole function: the CFG's blocks, each statement decomposed.
pub(super) fn lower_function(function: &Function) -> MirFunction {
    let graph = build_cfg(function);
    let mut lowering = MirLowering { next_temp: 0 };
    let blocks = graph.blocks.iter().map(|block| lower_block(block, &mut lowering)).collect();
    MirFunction { id: function.id, is_main: function.is_main, params: function.params.clone(), returns: function.returns, blocks }
}

fn lower_block(block: &CfgBlock, lowering: &mut MirLowering) -> MirBlock {
    let mut bindings = Vec::new();
    for statement in &block.statements {
        lower_statement(statement, &mut bindings, lowering);
    }
    MirBlock { id: block.id, statements: block.statements.clone(), bindings, terminator: block.terminator.clone() }
}

fn lower_statement(statement: &Statement, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) {
    match statement {
        Statement::Declare(id, expr) => {
            let rvalue = lower_expr(expr, bindings, lowering);
            bindings.push(MirBinding { target: MirPlace::Local(*id, expr_type(expr)), rvalue });
        }
        Statement::Assign(id, expr, _) => {
            let rvalue = lower_expr(expr, bindings, lowering);
            bindings.push(MirBinding { target: MirPlace::Local(*id, expr_type(expr)), rvalue });
        }
        Statement::FieldStore(id, fname, fty, value, base_ty) => {
            let rvalue = lower_expr(value, bindings, lowering);
            bindings.push(MirBinding {
                target: MirPlace::Field(Box::new(MirPlace::Local(*id, *base_ty)), fname.clone(), *fty),
                rvalue,
            });
        }
        Statement::Return(expr, _) => {
            let rvalue = lower_expr(expr, bindings, lowering);
            let temp = lowering.fresh(expr_type(expr));
            bindings.push(MirBinding { target: temp, rvalue });
        }
        Statement::Print(printed) => {
            if let Printed::Expr(e) = printed {
                let rvalue = lower_expr(e, bindings, lowering);
                let temp = lowering.fresh(expr_type(e));
                bindings.push(MirBinding { target: temp, rvalue });
            }
        }
        Statement::Frees(list) => {
            // A release list renders as an intrinsic with one operand per
            // slot, so the printed graph shows exactly which releases
            // survived drop elaboration (M2.4) — the same lists the C
            // backend emits — instead of a bare stmt() placeholder.
            let operands = list.iter().map(|(id, ty)| MirOperand::Place(MirPlace::Local(*id, *ty))).collect();
            let temp = lowering.fresh(Type::Int);
            bindings.push(MirBinding { target: temp, rvalue: MirRvalue::Intrinsic("releases", operands, Type::Int) });
        }
        other => {
            // Container updates, asserts, and the remaining statement shapes
            // render as named intrinsics with their slot and operands: the
            // MIR shows the operation the C backend owns end to end, without
            // inventing semantics for it. Control shapes (the CFG lowered
            // them into blocks and edges) keep the bare placeholder as a
            // tripwire: one should never reach a MIR block.
            let (name, operands) = statement_intrinsic(other, bindings, lowering);
            let temp = lowering.fresh(Type::Int);
            bindings.push(MirBinding { target: temp, rvalue: MirRvalue::Intrinsic(name, operands, Type::Int) });
        }
    }
}

fn lower_expr(expr: &Expr, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> MirRvalue {
    match expr {
        Expr::Number(n) => MirRvalue::Use(MirOperand::Int(*n)),
        Expr::FloatLit(x) => MirRvalue::Use(MirOperand::Float(*x)),
        Expr::BoolLit(b) => MirRvalue::Use(MirOperand::Bool(*b)),
        Expr::StrLit(text, line, col) => MirRvalue::Use(MirOperand::Str(Rc::from(text.as_str()), *line, *col)),
        Expr::Variable(id, ty, ..) => MirRvalue::Use(MirOperand::Place(MirPlace::Local(*id, *ty))),
        Expr::Field(base, fname, fty) => MirRvalue::Use(MirOperand::Place(MirPlace::Field(Box::new(base_place(base, bindings, lowering)), fname.clone(), *fty))),
        Expr::Binary(a, op, b, ..) if matches!(op, Kind::And | Kind::Or) => {
            // The right operand may trap or have side effects. Preserve its
            // source form, like a CFG condition, so it stays conditional.
            let lhs = lower_operand(a, bindings, lowering);
            MirRvalue::ShortCircuit(lhs, op.clone(), (**b).clone())
        }
        Expr::Binary(a, op, b, ..) => {
            let lhs = lower_operand(a, bindings, lowering);
            let rhs = lower_operand(b, bindings, lowering);
            MirRvalue::Binary(lhs, op.clone(), rhs, expr_type(expr))
        }
        Expr::Call(id, args, ret) => MirRvalue::Call(*id, lower_args(args, bindings, lowering), *ret),
        Expr::FnCall(id, args, ret, _) => MirRvalue::FnCall(*id, lower_args(args, bindings, lowering), *ret),
        Expr::ClosureLit(cid) => MirRvalue::ClosureLit(*cid),
        // The decomposed view records the spawn as a named side effect over
        // its capture slots and callable; the C emission goes through the
        // statement's original expression (M2.3's whole-shape rule).
        Expr::Spawn(slots, callable, tid, ..) => {
            let mut operands: Vec<MirOperand> = slots.iter().map(|slot| lower_operand(slot, bindings, lowering)).collect();
            operands.push(lower_operand(callable, bindings, lowering));
            MirRvalue::Intrinsic("thread_spawn", operands, Type::Thread(*tid))
        }
        Expr::Wait(handle, ret, ..) => MirRvalue::Intrinsic("thread_wait", vec![lower_operand(handle, bindings, lowering)], *ret),
        Expr::StructLit(sid, values) => MirRvalue::Aggregate(*sid, values.iter().map(|v| lower_expr(v, bindings, lowering)).collect()),
        Expr::EnumLit(name, eid, vidx, values) => MirRvalue::AdtAggregate(name, *eid, *vidx, values.iter().map(|v| lower_expr(v, bindings, lowering)).collect()),
        Expr::VecLit(values, ty, ..) => MirRvalue::VecNew(values.iter().map(|v| lower_expr(v, bindings, lowering)).collect(), *ty),
        other => {
            // Built-in reads and conversions render as named intrinsics
            // with their operands; an unhandled shape keeps the bare
            // placeholder so a missing arm is visible, not silent.
            let (name, operands) = expr_intrinsic(other, bindings, lowering);
            let ty = expr_type(other);
            let temp = lowering.fresh(ty.clone());
            bindings.push(MirBinding { target: temp.clone(), rvalue: MirRvalue::Intrinsic(name, operands, ty) });
            MirRvalue::Use(MirOperand::Place(temp))
        }
    }
}

fn lower_operand(expr: &Expr, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> MirOperand {
    match expr {
        Expr::Number(n) => MirOperand::Int(*n),
        Expr::FloatLit(x) => MirOperand::Float(*x),
        Expr::BoolLit(b) => MirOperand::Bool(*b),
        Expr::StrLit(text, line, col) => MirOperand::Str(Rc::from(text.as_str()), *line, *col),
        Expr::Variable(id, ty, ..) => MirOperand::Place(MirPlace::Local(*id, *ty)),
        other => {
            let rvalue = lower_expr(other, bindings, lowering);
            let temp = lowering.fresh(expr_type(other));
            bindings.push(MirBinding { target: temp.clone(), rvalue });
            MirOperand::Place(temp)
        }
    }
}

fn base_place(base: &Expr, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> MirPlace {
    match base {
        Expr::Variable(id, ty, ..) => MirPlace::Local(*id, *ty),
        Expr::Field(inner, fname, fty) => MirPlace::Field(Box::new(base_place(inner, bindings, lowering)), fname.clone(), *fty),
        other => {
            let rvalue = lower_expr(other, bindings, lowering);
            let temp = lowering.fresh(expr_type(other));
            bindings.push(MirBinding { target: temp.clone(), rvalue });
            temp
        }
    }
}

fn lower_args(args: &[Argument], bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> Vec<MirOperand> {
    args.iter().map(|arg| match arg {
        Argument::Value(e) => lower_operand(e, bindings, lowering),
        Argument::View(e) => {
            let value = lower_operand(e, bindings, lowering);
            if expr_type(e) == Type::StrView {
                return value;
            }
            let target = lowering.fresh(Type::StrView);
            bindings.push(MirBinding {
                target: target.clone(),
                rvalue: MirRvalue::Intrinsic("string_view", vec![value], Type::StrView),
            });
            MirOperand::Place(target)
        }
        Argument::Borrow(slot, take_address) => MirOperand::Borrow { slot: *slot, take_address: *take_address },
        Argument::FnPtr(id) => MirOperand::FnPtr(*id),
    }).collect()
}

// The named intrinsic for a statement the C backend owns end to end, with
// one operand per slot and sub-expression. Sub-expressions decompose through
// the same helpers the structured statements use, so the operands are the
// rvalues the program computes.
fn statement_intrinsic(statement: &Statement, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> (&'static str, Vec<MirOperand>) {
    match statement {
        Statement::Array(id, values) => {
            let mut operands = vec![MirOperand::Place(MirPlace::Local(*id, Type::Array(values.len())))];
            operands.extend(values.iter().map(|v| lower_operand(v, bindings, lowering)));
            ("array", operands)
        }
        Statement::Store(id, len, index, value, ..) => ("store", vec![
            MirOperand::Place(MirPlace::Local(*id, Type::Array(*len))),
            lower_operand(index, bindings, lowering),
            lower_operand(value, bindings, lowering),
        ]),
        Statement::VecPush(id, value, ..) => ("push", vec![MirOperand::Place(MirPlace::Local(*id, Type::Vec)), lower_operand(value, bindings, lowering)]),
        Statement::StrVecPush(id, value, ..) => ("push", vec![MirOperand::Place(MirPlace::Local(*id, Type::StrVec)), lower_operand(value, bindings, lowering)]),
        Statement::VecFPush(id, value, ..) => ("push", vec![MirOperand::Place(MirPlace::Local(*id, Type::VecF)), lower_operand(value, bindings, lowering)]),
        Statement::VecPop(id, ..) => ("pop", vec![MirOperand::Place(MirPlace::Local(*id, Type::Vec))]),
        Statement::StrVecPop(id, ..) => ("pop", vec![MirOperand::Place(MirPlace::Local(*id, Type::StrVec))]),
        Statement::VecFPop(id, ..) => ("pop", vec![MirOperand::Place(MirPlace::Local(*id, Type::VecF))]),
        Statement::VecStore(id, index, value, ..) => ("store", vec![MirOperand::Place(MirPlace::Local(*id, Type::Vec)), lower_operand(index, bindings, lowering), lower_operand(value, bindings, lowering)]),
        Statement::StrVecStore(id, index, value, ..) => ("store", vec![MirOperand::Place(MirPlace::Local(*id, Type::StrVec)), lower_operand(index, bindings, lowering), lower_operand(value, bindings, lowering)]),
        Statement::VecFStore(id, index, value, ..) => ("store", vec![MirOperand::Place(MirPlace::Local(*id, Type::VecF)), lower_operand(index, bindings, lowering), lower_operand(value, bindings, lowering)]),
        // The flag is the parser's sort-as-strings bit (vec[string] sorts by
        // bytes), so it names the intrinsic instead of posing as a direction.
        Statement::Sort(id, strings, ..) => (if *strings { "sort_strings" } else { "sort" }, vec![MirOperand::Place(MirPlace::Local(*id, if *strings { Type::StrVec } else { Type::Vec }))]),
        Statement::Reverse(id, strings, ..) => (if *strings { "reverse_strings" } else { "reverse" }, vec![MirOperand::Place(MirPlace::Local(*id, if *strings { Type::StrVec } else { Type::Vec }))]),
        Statement::MapPut(place, key, value, ..) => ("put", vec![map_place_operand(place), lower_operand(key, bindings, lowering), lower_operand(value, bindings, lowering)]),
        Statement::Assert(condition, ..) => ("assert", vec![lower_operand(condition, bindings, lowering)]),
        Statement::Print(Printed::Text(text)) => ("print", vec![MirOperand::Str(Rc::from(text.as_str()), 0, 0)]),
        // The match subject rides the switch terminator, so the payload
        // binding's operands are the binding itself and the variant indices.
        Statement::BindPayload(bid, _, variant, index, ty) => ("bind_payload", vec![
            MirOperand::Place(MirPlace::Local(*bid, ty.clone())),
            MirOperand::Int(*variant as i64),
            MirOperand::Int(*index as i64),
        ]),
        // The `?`'s error-path releases ride inside the statement (the C
        // backend emits them where the error path exits); the continuing
        // path's operand is the unwrapped expression.
        Statement::ResetTokens(cursor, iteration) => ("reset_tokens", vec![
            MirOperand::Place(MirPlace::Local(*iteration, Type::Tokens)),
            MirOperand::Place(MirPlace::Local(*cursor, Type::Tokens)),
        ]),
        Statement::AdvanceTokens(_, cursor, element, advanced) => ("advance_tokens", vec![
            MirOperand::Place(MirPlace::Local(*cursor, Type::Tokens)),
            MirOperand::Place(MirPlace::Local(*element, Type::StrView)),
            MirOperand::Place(MirPlace::Local(*advanced, Type::Bool)),
        ]),
        Statement::TakePropagationPayload(subject, ty, _, binding) => {
            let mut operands = vec![MirOperand::Place(MirPlace::Local(*subject, *ty))];
            if let Some((id, ty)) = binding {
                operands.push(MirOperand::Place(MirPlace::Local(*id, *ty)));
            }
            ("take_propagation_payload", operands)
        }
        Statement::Break => ("break", vec![]),
        Statement::Continue(label) => ("continue", label.iter().map(|l| MirOperand::Int(*l as i64)).collect()),
        // Control shapes never survive into a MIR block; a bare placeholder
        // here is a tripwire, not a rendering.
        _ => ("stmt", vec![]),
    }
}

fn map_kind_type(kind: &MapKind) -> Type {
    match kind {
        MapKind::Int => Type::Map,
        MapKind::Float => Type::MapF,
    }
}

fn map_place_operand(place: &MapPlace) -> MirOperand {
    match place {
        MapPlace::Var(id, kind) => MirOperand::Place(MirPlace::Local(*id, map_kind_type(kind))),
        MapPlace::Field(id, name, kind) => MirOperand::Place(MirPlace::Field(Box::new(MirPlace::Local(*id, map_kind_type(kind))), name.clone(), map_kind_type(kind))),
    }
}

// The named intrinsic for a built-in read or conversion, with one operand
// per input. Every arm mirrors what the C backend computes for the shape.
fn expr_intrinsic(expr: &Expr, bindings: &mut Vec<MirBinding>, lowering: &mut MirLowering) -> (&'static str, Vec<MirOperand>) {
    match expr {
        // M2.12 slice 2: the raw-pointer shapes ride the intrinsic path like
        // the built-in reads.
        Expr::BorrowAs(e, ..) => ("borrow_as_ptr", vec![lower_operand(e, bindings, lowering)]),
        Expr::DerefPtr(e, ..) => ("deref_ptr", vec![lower_operand(e, bindings, lowering)]),
        Expr::PtrOffset(e, c, bytewise, ..) => (
            if *bytewise { "byte_offset" } else { "ptr_offset" },
            vec![lower_operand(e, bindings, lowering), lower_operand(c, bindings, lowering)],
        ),
        Expr::PtrCast(e, mutable, _pointee_id, ..) => (
            if *mutable { "ptr_cast_mut" } else { "ptr_cast" },
            vec![lower_operand(e, bindings, lowering)],
        ),
        Expr::PtrDropInPlace(e, ..) => ("ptr_drop_in_place", vec![lower_operand(e, bindings, lowering)]),
        Expr::PtrWrite(p, v, ..) => ("ptr_write", vec![lower_operand(p, bindings, lowering), lower_operand(v, bindings, lowering)]),
        Expr::Len(e, ..) => ("len", vec![lower_operand(e, bindings, lowering)]),
        Expr::Index(id, len, index, ..) => ("index", vec![
            MirOperand::Place(MirPlace::Local(*id, Type::Array(*len))),
            lower_operand(index, bindings, lowering),
        ]),
        Expr::VecIndex(id, index, _, _, borrowed, element) => {
            let vector = match (*element, *borrowed) {
                (Type::F64, false) => Type::VecF,
                (Type::F64, true) => Type::VecFRef,
                (_, false) => Type::Vec,
                (_, true) => Type::VecRef,
            };
            ("index", vec![MirOperand::Place(MirPlace::Local(*id, vector)), lower_operand(index, bindings, lowering)])
        }
        Expr::StrVecIndex(id, index, _, _, borrowed) => {
            let vector = if *borrowed { Type::StrVecRef } else { Type::StrVec };
            ("index", vec![MirOperand::Place(MirPlace::Local(*id, vector)), lower_operand(index, bindings, lowering)])
        }
        Expr::Join(id, sep, ..) => ("join", vec![MirOperand::Place(MirPlace::Local(*id, Type::StrVec)), lower_operand(sep, bindings, lowering)]),
        Expr::Split(base, sep, ..) => ("split", vec![lower_operand(base, bindings, lowering), lower_operand(sep, bindings, lowering)]),
        Expr::Args(..) => ("args", vec![]),
        Expr::ReadFile(path, ..) => ("read_file", vec![lower_operand(path, bindings, lowering)]),
        Expr::ReadFileBytes(path, offset, limit, ..) => ("read_file_bytes", vec![
            lower_operand(path, bindings, lowering), lower_operand(offset, bindings, lowering),
            lower_operand(limit, bindings, lowering),
        ]),
        Expr::WriteFile(path, text, ..) => ("write_file", vec![lower_operand(path, bindings, lowering), lower_operand(text, bindings, lowering)]),
        Expr::Bytes(e, ..) => ("bytes", vec![lower_operand(e, bindings, lowering)]),
        Expr::Contains(haystack, needle, ..) => ("contains", vec![lower_operand(haystack, bindings, lowering), lower_operand(needle, bindings, lowering)]),
        Expr::Slice(base, start, end, ..) => ("slice", vec![lower_operand(base, bindings, lowering), lower_operand(start, bindings, lowering), lower_operand(end, bindings, lowering)]),
        Expr::SliceView(id, field, start, end, ..) => ("slice_view", vec![string_base_operand(*id, field), lower_operand(start, bindings, lowering), lower_operand(end, bindings, lowering)]),
        Expr::Tokens(id, field, sep, ..) => ("tokens", vec![string_base_operand(*id, field), lower_operand(sep, bindings, lowering)]),
        Expr::ToF64(e, ..) => ("to_f64", vec![lower_operand(e, bindings, lowering)]),
        Expr::ToI64(e, ..) => ("to_i64", vec![lower_operand(e, bindings, lowering)]),
        Expr::FloatToString(e, ..) => ("float_to_string", vec![lower_operand(e, bindings, lowering)]),
        Expr::StrEq(a, b, ..) => ("str_eq", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::MapKeys(place, ..) => ("keys", vec![map_place_operand(place)]),
        Expr::RunCommand(program, vid, ..) => ("run_command", vec![lower_operand(program, bindings, lowering), MirOperand::Place(MirPlace::Local(*vid, Type::StrVec))]),
        Expr::ToString(e, ..) => ("to_string", vec![lower_operand(e, bindings, lowering)]),
        Expr::Lines(e, ..) => ("lines", vec![lower_operand(e, bindings, lowering)]),
        Expr::CountLinesWith(text, pattern, ..) => ("count_lines_with", vec![lower_operand(text, bindings, lowering), lower_operand(pattern, bindings, lowering)]),
        Expr::TryReadFile(path, ..) => ("read_file", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryListDir(path, ..) => ("list_dir", vec![lower_operand(path, bindings, lowering)]),
        Expr::PathExists(path, ..) => ("path_exists", vec![lower_operand(path, bindings, lowering)]),
        Expr::PathIsDir(path, ..) => ("path_is_dir", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryWriteFile(path, text, ..) => ("write_file", vec![lower_operand(path, bindings, lowering), lower_operand(text, bindings, lowering)]),
        Expr::ParseInt(text, ..) => ("parse_int", vec![lower_operand(text, bindings, lowering)]),
        Expr::PathJoin(a, b, ..) => ("path_join", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::PathName(path, ..) => ("path_name", vec![lower_operand(path, bindings, lowering)]),
        Expr::PathParent(path, ..) => ("path_parent", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryFileInfo(path, ..) => ("file_info", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryCopyFile(a, b, ..) => ("copy_file", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::TryRenamePath(a, b, ..) => ("rename_path", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::TryRemoveFile(path, ..) => ("remove_file", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryRemoveDir(path, ..) => ("remove_dir", vec![lower_operand(path, bindings, lowering)]),
        Expr::TryCreateDir(path, ..) => ("create_dir", vec![lower_operand(path, bindings, lowering)]),
        Expr::SetCwd(path, ..) => ("set_cwd", vec![lower_operand(path, bindings, lowering)]),
        Expr::MapNew(..) => ("map_new", vec![]),
        Expr::MapGet(place, key, ..) => ("map_get", vec![map_place_operand(place), lower_operand(key, bindings, lowering)]),
        Expr::MapHas(place, key, ..) => ("map_has", vec![map_place_operand(place), lower_operand(key, bindings, lowering)]),
        Expr::MapRemove(place, key, ..) => ("map_remove", vec![map_place_operand(place), lower_operand(key, bindings, lowering)]),
        Expr::MapInc(place, key, value, ..) => ("map_inc", vec![map_place_operand(place), lower_operand(key, bindings, lowering), lower_operand(value, bindings, lowering)]),
        Expr::Utf8Len(e, ..) => ("utf8_len", vec![lower_operand(e, bindings, lowering)]),
        Expr::Utf8Prefix(e, n, ..) => ("utf8_prefix", vec![lower_operand(e, bindings, lowering), lower_operand(n, bindings, lowering)]),
        Expr::Trim(e, ..) => ("trim", vec![lower_operand(e, bindings, lowering)]),
        Expr::StartsWith(a, b, ..) => ("starts_with", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::EndsWith(a, b, ..) => ("ends_with", vec![lower_operand(a, bindings, lowering), lower_operand(b, bindings, lowering)]),
        Expr::TryAppendFile(path, text, ..) => ("append_file", vec![lower_operand(path, bindings, lowering), lower_operand(text, bindings, lowering)]),
        Expr::Env(name, ..) => ("env", vec![lower_operand(name, bindings, lowering)]),
        Expr::JsonGet(src, key, ..) => ("json_get", vec![lower_operand(src, bindings, lowering), lower_operand(key, bindings, lowering)]),
        Expr::JsonInt(src, key, ..) => ("json_int", vec![lower_operand(src, bindings, lowering), lower_operand(key, bindings, lowering)]),
        Expr::JsonBool(src, key, ..) => ("json_bool", vec![lower_operand(src, bindings, lowering), lower_operand(key, bindings, lowering)]),
        Expr::JsonLen(src, key, ..) => ("json_len", vec![lower_operand(src, bindings, lowering), lower_operand(key, bindings, lowering)]),
        Expr::JsonHas(src, key, ..) => ("json_has", vec![lower_operand(src, bindings, lowering), lower_operand(key, bindings, lowering)]),
        Expr::Find(text, pattern, ..) => ("find", vec![lower_operand(text, bindings, lowering), lower_operand(pattern, bindings, lowering)]),
        Expr::ReadStdin(..) => ("read_stdin", vec![]),
        Expr::Cwd(..) => ("cwd", vec![]),
        Expr::StrVecNew(..) => ("strvec_new", vec![]),
        Expr::F64VecNew(..) => ("f64vec_new", vec![]),
        Expr::NowMs => ("now_ms", vec![]),
        Expr::NowS => ("now_s", vec![]),
        Expr::Neg(e, ..) => ("neg", vec![lower_operand(e, bindings, lowering)]),
        Expr::Not(e, ..) => ("not", vec![lower_operand(e, bindings, lowering)]),
        // The math calls are indexed into a name table; the printed name is
        // the source spelling, and the operands are the call's arguments.
        Expr::MathCall(index, args, ..) => {
            let mut operands: Vec<MirOperand> = args.iter().map(|a| lower_operand(a, bindings, lowering)).collect();
            operands.insert(0, MirOperand::Str(Rc::from(super::MATH_FNS[*index as usize].0), 0, 0));
            ("math", operands)
        }
        _ => ("expr", vec![]),
    }
}

// A slice view or token cursor reads a string variable, or a string field of
// one; the field name rides along when present.
fn string_base_operand(id: usize, field: &Option<String>) -> MirOperand {
    match field {
        Some(name) => MirOperand::Place(MirPlace::Field(Box::new(MirPlace::Local(id, Type::String)), name.clone(), Type::String)),
        None => MirOperand::Place(MirPlace::Local(id, Type::String)),
    }
}

pub(super) fn expr_type(expr: &Expr) -> Type {
    match expr {
        Expr::Spawn(_, _, tid, ..) => Type::Thread(*tid),
        Expr::Wait(_, ret, ..) => *ret,
        Expr::AtomicNew(..) => Type::Atomic,
        Expr::AtomicLoad(..) | Expr::AtomicAdd(..) => Type::Int,
        Expr::AtomicCas(..) => Type::Bool,
        Expr::SharedNew(_, sid) => Type::Shared(*sid),
        Expr::SharedGet(_, sid) => shared_types_get(*sid),
        Expr::MutexNew => Type::Mutex,
        Expr::MutexLock(..) => Type::Guard,
        Expr::ChanNew(..) => Type::Chan,
        Expr::ChanSend(..) | Expr::ChanTrySend(..) => Type::Bool,
        Expr::ChanRecv(..) | Expr::ChanSelect(..) | Expr::ExecRun(..) | Expr::PoolJoin(..) => Type::Int,
        Expr::ExecNew => Type::Exec,
        Expr::ScopeNew => Type::TaskScope,
        Expr::PoolNew(..) => Type::Pool,
        Expr::Number(..) | Expr::Index(..) | Expr::Len(..) => Type::Int,
        Expr::VecIndex(_, _, _, _, _, ty) => *ty,
        Expr::StrVecIndex(..) => Type::String,
        Expr::Binary(_, _, _, _, _, ty) => *ty,
        Expr::FloatLit(..) | Expr::ToF64(..) => Type::F64,
        Expr::BoolLit(..) | Expr::StrEq(..) | Expr::Contains(..) => Type::Bool,
        Expr::Variable(_, ty, ..) | Expr::Field(_, _, ty) | Expr::VecLit(_, ty, ..) => *ty,
        Expr::StructLit(sid, _) => Type::Struct(*sid),
        Expr::EnumLit(name, layout, ..) => Type::Enum(name, *layout),
        Expr::Call(_, _, ty) | Expr::FnCall(_, _, ty, _) => *ty,
        // The printed place types are evidence, so the string-shaped
        // expressions name their real types instead of the integer
        // catch-all (a string literal bound to `v0 (i64)` misleads).
        Expr::StrLit(..) | Expr::ReadFile(..) | Expr::Join(..) | Expr::Trim(..)
        | Expr::Slice(..) | Expr::FloatToString(..) | Expr::ToString(..)
        | Expr::PathJoin(..) | Expr::PathName(..) | Expr::PathParent(..)
        | Expr::Utf8Prefix(..) => Type::String,
        Expr::Bytes(..) | Expr::ReadFileBytes(..) => Type::Vec,
        Expr::Neg(value, ..) => expr_type(value),
        Expr::ClosureLit(id) => Type::Closure(*id),
        Expr::MapInc(place, ..) if place.f64_value() => Type::F64,
        Expr::MapHas(..) | Expr::MapRemove(..) => Type::Bool,
        Expr::SliceView(..) => Type::StrView,
        Expr::Tokens(..) => Type::Tokens,
        Expr::Split(..) | Expr::Args(..) | Expr::Lines(..) | Expr::StrVecNew(..) | Expr::MapKeys(..) => Type::StrVec,
        Expr::F64VecNew(..) => Type::VecF,
        Expr::MapNew(_, _, MapKind::Float) => Type::MapF,
        Expr::MapNew(..) => Type::Map,
        // Math calls return f64 by definition (ADR-60).
        Expr::MathCall(..) => Type::F64,
        Expr::Not(..) | Expr::PathExists(..) | Expr::PathIsDir(..) | Expr::StartsWith(..) | Expr::EndsWith(..) | Expr::JsonHas(..) => Type::Bool,
        // M2.12 slice 2: the address takes the pointee's pointer shape; a
        // deref yields the pointee.
        Expr::BorrowAs(e, mutable, ..) => if *mutable { Type::PtrMut(crate::frontend::intern_pointee(expr_type(e))) } else { Type::Ptr(crate::frontend::intern_pointee(expr_type(e))) },
        // Both pointer forms deref to their pointee — PtrMut was missing
        // here, so a *mut f64 deref printed through the i64 printer (the
        // slice-3 shapes were i64-only and matched the fallback by luck).
        Expr::DerefPtr(p, ..) => match expr_type(p) { Type::Ptr(id) | Type::PtrMut(id) => crate::frontend::ptr_pointee(id), other => other },
        // Offsetting keeps the base's pointer type.
        Expr::PtrOffset(p, ..) => expr_type(p),
        Expr::PtrDropInPlace(..) | Expr::PtrWrite(..) => Type::Int,
        // A retag's type is the new pointer, taken from the variant's own
        // fields — recursing on the same expression here would not terminate.
        Expr::PtrCast(_, mutable, pointee_id, ..) => {
            if *mutable { Type::PtrMut(*pointee_id) } else { Type::Ptr(*pointee_id) }
        }
        // Result-returning built-ins carry their result type; the printed
        // temp names the enum the program must match on.
        Expr::TryReadFile(_, ty, ..) | Expr::TryListDir(_, ty, ..) | Expr::ParseInt(_, ty, ..)
        | Expr::TryFileInfo(_, ty, ..) | Expr::TryRemoveFile(_, ty, ..) | Expr::TryRemoveDir(_, ty, ..)
        | Expr::TryCreateDir(_, ty, ..) | Expr::SetCwd(_, ty, ..) | Expr::Env(_, ty, ..)
        | Expr::RunCommand(_, _, ty, ..) | Expr::MapGet(_, _, ty, ..)
        | Expr::ReadStdin(ty, ..) | Expr::Cwd(ty, ..) => ty.clone(),
        Expr::TryWriteFile(_, _, ty, ..) | Expr::TryCopyFile(_, _, ty, ..) | Expr::TryRenamePath(_, _, ty, ..)
        | Expr::TryAppendFile(_, _, ty, ..) | Expr::JsonGet(_, _, ty, ..) | Expr::JsonInt(_, _, ty, ..)
        | Expr::JsonBool(_, _, ty, ..) | Expr::JsonLen(_, _, ty, ..) | Expr::Find(_, _, ty, ..) => ty.clone(),
        Expr::ToI64(..) | Expr::WriteFile(..) | Expr::CountLinesWith(..) | Expr::Utf8Len(..)
        | Expr::MapInc(..) | Expr::NowMs | Expr::NowS => Type::Int,
    }
}

// M2.10 MIR Constant Propagation and Folding + Dead Temporary Elimination.
// Performs block-local constant propagation and checked integer/boolean
// constant folding, preserves checked overflow/division-by-zero semantics
// by leaving trapping operations unfolded, folds constant Branch conditions
// into unconditional Goto terminators, and eliminates unreferenced pure
// temporaries.
pub(super) fn optimize_function(function: &mut MirFunction) {
    for block in &mut function.blocks {
        optimize_block(block);
    }
    simplify_cfg(function);
}

// M2.4: drop elaboration over the optimized graph. Removing a release list can
// leave a block with no statements at all, which is exactly the trampoline
// shape the edge simplifier threads through, so it runs once more afterwards.
// Neither pass can invalidate the other: threading and fusion only retarget
// edges and preserve statement order, and a pruned block is unreachable.
pub(super) fn elaborate_drops(function: &mut MirFunction) -> drops::DropStats {
    let stats = drops::elaborate(function);
    // Same diagnostic channel as the phase split (M2.4 evidence): counts of
    // releases kept, kept without proof, and proven redundant, per function.
    if std::env::var_os("AE_DROP_STATS").is_some() {
        eprintln!(
            "ae drops: fn #{} kept={} kept_mixed={} elided_moved={} elided_uninit={}",
            function.id, stats.kept, stats.kept_mixed, stats.elided_dropped, stats.elided_uninit
        );
    }
    simplify_cfg(function);
    stats
}

// M2.10 MIR Unreachable Block Elimination and CFG simplification.
//
// Control-flow lowering creates blocks no path reaches: the continuation a
// fully-returning arm abandoned, the join every arm returned past, the
// pending tail of a function that always returns. Constant branch folding
// (above) orphans more of them. This pass threads edges through empty
// trampoline blocks, collapses a branch whose resolved targets coincide
// (only when the condition is side-effect free), drops every block that is
// unreachable from the entry, and fuses a block into the one block that
// unconditionally jumps to it.
//
// Block ids stay stable — nothing is renumbered — so printed MIR keeps its
// block names and every surviving edge keeps pointing at the same block.
// `validate` therefore checks id membership rather than a dense range.
// Dropping a block no terminator points at, and retargeting an edge through a
// block that carries no bindings and only a `Goto`, cannot change behavior.
fn simplify_cfg(function: &mut MirFunction) {
    // Each round either retargets an edge or removes a block; both are finite,
    // but pruning can expose a new trampoline, so iterate to a fixed point.
    let rounds = function.blocks.len() + 1;
    for _ in 0..rounds {
        let threaded = thread_edges(&mut function.blocks);
        let pruned = retain_reachable(&mut function.blocks);
        let merged = merge_linear_blocks(&mut function.blocks);
        if !threaded && !pruned && !merged {
            return;
        }
    }
}

// Resolve an edge target through a chain of empty `Goto` blocks. A cycle of
// empty blocks resolves to the first block already visited, so threading
// never loops forever and never rewrites an infinite loop into a different
// one.
fn resolve_target(target: usize, trampolines: &HashMap<usize, usize>) -> usize {
    let mut current = target;
    let mut seen = HashSet::new();
    while let Some(next) = trampolines.get(&current) {
        if !seen.insert(current) {
            return current;
        }
        current = *next;
    }
    current
}

fn thread_edges(blocks: &mut [MirBlock]) -> bool {
    // A trampoline is empty in BOTH views: a block can carry statements but
    // no bindings (a literal `print` binds nothing), and threading an edge
    // around it would delete the print. Only a block with neither
    // statements nor bindings may vanish under an edge.
    let trampolines: HashMap<usize, usize> = blocks
        .iter()
        .filter(|block| block.bindings.is_empty() && block.statements.is_empty())
        .filter_map(|block| match block.terminator {
            CfgTerminator::Goto(next) => Some((block.id, next)),
            _ => None,
        })
        .collect();
    let mut changed = false;
    for block in blocks.iter_mut() {
        let replacement = match &block.terminator {
            CfgTerminator::Goto(target) => {
                let resolved = resolve_target(*target, &trampolines);
                (resolved != *target).then(|| CfgTerminator::Goto(resolved))
            }
            CfgTerminator::Branch(cond, then_target, else_target) => {
                let then_target_now = resolve_target(*then_target, &trampolines);
                let else_target_now = resolve_target(*else_target, &trampolines);
                if then_target_now == else_target_now && condition_is_pure(cond) {
                    Some(CfgTerminator::Goto(then_target_now))
                } else if then_target_now != *then_target || else_target_now != *else_target {
                    Some(CfgTerminator::Branch(cond.clone(), then_target_now, else_target_now))
                } else {
                    None
                }
            }
            CfgTerminator::Switch(subject, layout, targets) => {
                let resolved: Vec<usize> = targets.iter().map(|t| resolve_target(*t, &trampolines)).collect();
                (resolved != *targets).then(|| CfgTerminator::Switch(*subject, *layout, resolved))
            }
            _ => None,
        };
        if let Some(terminator) = replacement {
            block.terminator = terminator;
            changed = true;
        }
    }
    changed
}

// A condition may be dropped only when evaluating it cannot be observed.
// Conditions keep their whole source form in the CFG, so a call in one is a
// real side effect (§2.2 Rule 7: optimizations must not change behavior).
fn condition_is_pure(expr: &Expr) -> bool {
    match expr {
        Expr::Number(..) | Expr::FloatLit(..) | Expr::BoolLit(..) => true,
        Expr::Variable(_, Type::Int | Type::Bool | Type::F64, ..) => true,
        Expr::Not(value) => condition_is_pure(value),
        Expr::Binary(left, op, right, ..) => {
            // Arithmetic can trap on overflow or division by zero.
            matches!(op, Kind::And | Kind::Or | Kind::EqEq | Kind::NotEq
                | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq)
                && condition_is_pure(left) && condition_is_pure(right)
        }
        _ => false,
    }
}

fn retain_reachable(blocks: &mut Vec<MirBlock>) -> bool {
    let Some(entry) = blocks.first().map(|block| block.id) else { return false };
    let successors: HashMap<usize, Vec<usize>> = blocks
        .iter()
        .map(|block| (block.id, terminator_targets(&block.terminator)))
        .collect();
    let mut reachable: HashSet<usize> = HashSet::new();
    let mut pending = vec![entry];
    while let Some(id) = pending.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if let Some(next) = successors.get(&id) {
            pending.extend(next.iter().copied());
        }
    }
    let before = blocks.len();
    blocks.retain(|block| reachable.contains(&block.id));
    blocks.len() != before
}

// Fuse a block into the block that unconditionally jumps to it.
//
// `A --goto--> B` where B's only incoming edge is A's: every execution of B's
// bindings is immediately preceded by all of A's, so concatenating the binding
// lists into A and taking B's terminator preserves order, temporaries (ids are
// unique per function), and control flow, while removing one block and one
// edge. The entry block is never fused away because control also arrives there
// implicitly, and a self-loop is left alone.
//
// Only unconditional edges qualify. A branch's arm executes conditionally, so
// folding its bindings into the branch block would run them on the other path
// as well.
fn merge_linear_blocks(blocks: &mut Vec<MirBlock>) -> bool {
    let Some(entry) = blocks.first().map(|block| block.id) else { return false };
    let mut merged_any = false;
    loop {
        // Edge counts: a block is a fusion target only when exactly one edge
        // reaches it, and that edge is the unconditional jump we fuse over.
        let mut incoming: HashMap<usize, usize> = HashMap::new();
        for block in blocks.iter() {
            for target in terminator_targets(&block.terminator) {
                *incoming.entry(target).or_insert(0) += 1;
            }
        }
        let mut candidate = None;
        for block in blocks.iter() {
            let target = match block.terminator {
                CfgTerminator::Goto(target) => target,
                _ => continue,
            };
            if target == block.id || target == entry { continue; }
            if incoming.get(&target).copied().unwrap_or(0) != 1 { continue; }
            candidate = Some((block.id, target));
            break;
        }
        let Some((head_id, tail_id)) = candidate else { return merged_any };
        let Some(tail_index) = blocks.iter().position(|block| block.id == tail_id) else { return merged_any };
        let tail = blocks.remove(tail_index);
        let head_index = blocks.iter().position(|block| block.id == head_id).expect("fusing block present");
        let head = &mut blocks[head_index];
        head.statements.extend(tail.statements);
        head.bindings.extend(tail.bindings);
        head.terminator = tail.terminator;
        merged_any = true;
    }
}

fn terminator_targets(terminator: &CfgTerminator) -> Vec<usize> {
    match terminator {
        CfgTerminator::Goto(target) => vec![*target],
        CfgTerminator::Branch(_, a, b) => vec![*a, *b],
        CfgTerminator::Switch(_, _, targets) => targets.clone(),
        CfgTerminator::Return(_) | CfgTerminator::Exit(_) | CfgTerminator::Unreachable => Vec::new(),
    }
}

fn optimize_block(block: &mut MirBlock) {
    let mut local_consts: HashMap<usize, MirOperand> = HashMap::new();
    let mut temp_consts: HashMap<usize, MirOperand> = HashMap::new();

    for binding in &mut block.bindings {
        // Calls and intrinsics can write through pointers. Aggregate
        // operands can contain calls directly. Drop local facts at such
        // operations until MIR can prove which places they write.
        if !matches!(binding.rvalue, MirRvalue::Use(_) | MirRvalue::Binary(..) | MirRvalue::ClosureLit(_)) {
            local_consts.clear();
        }
        binding.rvalue = fold_rvalue(&binding.rvalue, &local_consts, &temp_consts);
        match &binding.target {
            MirPlace::Local(id, _) => {
                if let MirRvalue::Use(op) = &binding.rvalue {
                    if is_scalar_const(op) {
                        local_consts.insert(*id, op.clone());
                    } else {
                        local_consts.remove(id);
                    }
                } else {
                    local_consts.remove(id);
                }
            }
            MirPlace::Temp(id, _) => {
                if let MirRvalue::Use(op) = &binding.rvalue {
                    if is_scalar_const(op) {
                        temp_consts.insert(*id, op.clone());
                    }
                }
            }
            MirPlace::Field(base, _, _) => {
                if let Some(root) = root_local(base) {
                    local_consts.remove(&root);
                }
            }
        }
    }

    // Fold constant branch terminators into unconditional Goto edges when the
    // condition is a known boolean constant or a local known to hold one.
    if let CfgTerminator::Branch(cond, then_target, else_target) = &block.terminator {
        let known_bool = match cond {
            Expr::BoolLit(b) => Some(*b),
            Expr::Variable(id, Type::Bool, ..) => match local_consts.get(id) {
                Some(MirOperand::Bool(b)) => Some(*b),
                _ => None,
            },
            _ => None,
        };
        if let Some(branch_taken) = known_bool {
            block.terminator = CfgTerminator::Goto(if branch_taken { *then_target } else { *else_target });
        }
    }

    // Dead temporary elimination: remove `Temp` bindings that are pure (`Use`
    // or `Binary` that already folded to `Use`) and never referenced by any
    // subsequent binding in the block. (The last binding of a statement may
    // represent a statement-level effect like `Return` or `Print`, so only
    // intermediate temporaries produced during expression decomposition that
    // become unused after folding are removed.)
    let mut used_temps: HashSet<usize> = HashSet::new();
    for binding in &block.bindings {
        collect_rvalue_temps(&binding.rvalue, &mut used_temps);
        if let MirPlace::Field(base, _, _) = &binding.target {
            collect_place_temps(base, &mut used_temps);
        }
    }
    // Keep the final binding if it targets a Temp (statement effect sink), and
    // any Temp that holds a side-effecting rvalue (Call, FnCall, Intrinsic).
    let last_idx = block.bindings.len().saturating_sub(1);
    let mut retained = Vec::with_capacity(block.bindings.len());
    for (idx, binding) in block.bindings.drain(..).enumerate() {
        if let MirPlace::Temp(tid, _) = &binding.target {
            let is_pure = matches!(binding.rvalue, MirRvalue::Use(_));
            if idx < last_idx && is_pure && !used_temps.contains(tid) {
                continue;
            }
        }
        retained.push(binding);
    }
    block.bindings = retained;
}

fn is_scalar_const(op: &MirOperand) -> bool {
    matches!(op, MirOperand::Int(_) | MirOperand::Bool(_))
}

fn root_local(place: &MirPlace) -> Option<usize> {
    match place {
        MirPlace::Local(id, _) => Some(*id),
        MirPlace::Field(base, _, _) => root_local(base),
        MirPlace::Temp(_, _) => None,
    }
}

fn subst_operand(op: &MirOperand, locals: &HashMap<usize, MirOperand>, temps: &HashMap<usize, MirOperand>) -> MirOperand {
    match op {
        MirOperand::Place(MirPlace::Local(id, _)) => locals.get(id).cloned().unwrap_or_else(|| op.clone()),
        MirOperand::Place(MirPlace::Temp(id, _)) => temps.get(id).cloned().unwrap_or_else(|| op.clone()),
        _ => op.clone(),
    }
}

fn fold_rvalue(rvalue: &MirRvalue, locals: &HashMap<usize, MirOperand>, temps: &HashMap<usize, MirOperand>) -> MirRvalue {
    match rvalue {
        MirRvalue::Use(op) => MirRvalue::Use(subst_operand(op, locals, temps)),
        MirRvalue::ShortCircuit(lhs, op, rhs) => {
            MirRvalue::ShortCircuit(subst_operand(lhs, locals, temps), op.clone(), rhs.clone())
        }
        MirRvalue::Binary(lhs, op, rhs, ty) => {
            let l = subst_operand(lhs, locals, temps);
            let r = subst_operand(rhs, locals, temps);
            if let Some(folded) = eval_binary(&l, op, &r) {
                MirRvalue::Use(folded)
            } else {
                MirRvalue::Binary(l, op.clone(), r, *ty)
            }
        }
        MirRvalue::Call(id, args, ret) => MirRvalue::Call(*id, args.iter().map(|a| subst_operand(a, locals, temps)).collect(), *ret),
        MirRvalue::FnCall(id, args, ret) => MirRvalue::FnCall(*id, args.iter().map(|a| subst_operand(a, locals, temps)).collect(), *ret),
        MirRvalue::ClosureLit(id) => MirRvalue::ClosureLit(*id),
        MirRvalue::Intrinsic(name, args, ret) => MirRvalue::Intrinsic(name, args.iter().map(|a| subst_operand(a, locals, temps)).collect(), *ret),
        MirRvalue::Aggregate(sid, vals) => MirRvalue::Aggregate(*sid, vals.iter().map(|v| fold_rvalue(v, locals, temps)).collect()),
        MirRvalue::AdtAggregate(name, eid, vidx, vals) => MirRvalue::AdtAggregate(name, *eid, *vidx, vals.iter().map(|v| fold_rvalue(v, locals, temps)).collect()),
        MirRvalue::VecNew(vals, ty) => MirRvalue::VecNew(vals.iter().map(|v| fold_rvalue(v, locals, temps)).collect(), *ty),
    }
}

// Checked constant evaluation (§2.2 Rule 7: optimizations must not change
// observable behavior; overflow and division/modulo by zero stay unfolded).
fn eval_binary(lhs: &MirOperand, op: &Kind, rhs: &MirOperand) -> Option<MirOperand> {
    match (lhs, rhs) {
        (MirOperand::Int(a), MirOperand::Int(b)) => match op {
            Kind::Plus => a.checked_add(*b).map(MirOperand::Int),
            Kind::Minus => a.checked_sub(*b).map(MirOperand::Int),
            Kind::Star => a.checked_mul(*b).map(MirOperand::Int),
            Kind::Slash => if *b != 0 { a.checked_div(*b).map(MirOperand::Int) } else { None },
            Kind::Percent => if *b != 0 { a.checked_rem(*b).map(MirOperand::Int) } else { None },
            Kind::Pipe => Some(MirOperand::Int(a | b)),
            Kind::Caret => Some(MirOperand::Int(a ^ b)),
            Kind::Amp => Some(MirOperand::Int(a & b)),
            Kind::Shl => Some(MirOperand::Int(a.wrapping_shl((*b as u32) & 63))),
            Kind::Shr => Some(MirOperand::Int(a.wrapping_shr((*b as u32) & 63))),
            Kind::EqEq => Some(MirOperand::Bool(a == b)),
            Kind::NotEq => Some(MirOperand::Bool(a != b)),
            Kind::Less => Some(MirOperand::Bool(a < b)),
            Kind::LessEq => Some(MirOperand::Bool(a <= b)),
            Kind::Greater => Some(MirOperand::Bool(a > b)),
            Kind::GreaterEq => Some(MirOperand::Bool(a >= b)),
            _ => None,
        },
        (MirOperand::Bool(a), MirOperand::Bool(b)) => match op {
            Kind::And => Some(MirOperand::Bool(*a && *b)),
            Kind::Or => Some(MirOperand::Bool(*a || *b)),
            Kind::EqEq => Some(MirOperand::Bool(a == b)),
            Kind::NotEq => Some(MirOperand::Bool(a != b)),
            _ => None,
        },
        _ => None,
    }
}

fn collect_rvalue_temps(rvalue: &MirRvalue, used: &mut HashSet<usize>) {
    match rvalue {
        MirRvalue::Use(op) => collect_operand_temps(op, used),
        MirRvalue::ShortCircuit(lhs, ..) => collect_operand_temps(lhs, used),
        MirRvalue::Binary(a, _, b, _) => { collect_operand_temps(a, used); collect_operand_temps(b, used); }
        MirRvalue::Call(_, args, _) | MirRvalue::FnCall(_, args, _) | MirRvalue::Intrinsic(_, args, _) => {
            for a in args { collect_operand_temps(a, used); }
        }
        MirRvalue::Aggregate(_, vals) | MirRvalue::AdtAggregate(_, _, _, vals) | MirRvalue::VecNew(vals, _) => {
            for v in vals { collect_rvalue_temps(v, used); }
        }
        MirRvalue::ClosureLit(_) => {}
    }
}

fn collect_operand_temps(op: &MirOperand, used: &mut HashSet<usize>) {
    if let MirOperand::Place(place) = op {
        collect_place_temps(place, used);
    }
}

fn collect_place_temps(place: &MirPlace, used: &mut HashSet<usize>) {
    match place {
        MirPlace::Temp(id, _) => { used.insert(*id); }
        MirPlace::Field(base, _, _) => collect_place_temps(base, used),
        MirPlace::Local(_, _) => {}
    }
}

// Check block identity, successor references and the declared type of each
// call result before a consumer uses the optimized graph.
pub(super) fn validate(function: &MirFunction) -> Result<(), String> {
    // Unreachable-block elimination leaves the surviving ids sparse, so a
    // target is valid when it names a surviving block, not when it indexes
    // the block vector.
    let ids: HashSet<usize> = function.blocks.iter().map(|block| block.id).collect();
    if function.blocks.first().map(|block| block.id) != Some(0) || ids.len() != function.blocks.len() {
        return Err(format!("internal: fn #{} has a missing entry block or duplicate block IDs", function.id));
    }
    let mut all_temps = HashSet::new();
    let mut defined = HashSet::new();
    let mut used = HashSet::new();
    for block in &function.blocks {
        defined.clear();
        for binding in &block.bindings {
            used.clear();
            collect_rvalue_temps(&binding.rvalue, &mut used);
            if let MirPlace::Field(base, ..) = &binding.target {
                collect_place_temps(base, &mut used);
            }
            if let Some(id) = used.difference(&defined).min() {
                return Err(format!(
                    "internal: fn #{} block {} uses undefined temporary t{id}",
                    function.id, block.id,
                ));
            }
            if let MirPlace::Temp(id, _) = binding.target {
                if !all_temps.insert(id) {
                    return Err(format!("internal: fn #{} defines temporary t{id} twice", function.id));
                }
                defined.insert(id);
            }

            let returns = match &binding.rvalue {
                MirRvalue::Call(_, _, ty) | MirRvalue::FnCall(_, _, ty) => ty,
                _ => continue,
            };
            let target = match &binding.target {
                MirPlace::Local(_, ty) | MirPlace::Field(_, _, ty) | MirPlace::Temp(_, ty) => ty,
            };
            if target != returns {
                return Err(format!(
                    "internal: fn #{} block {} assigns a call result of type {} to a place of type {}",
                    function.id, block.id, returns.name(), target.name(),
                ));
            }
        }
        match &block.terminator {
            CfgTerminator::Goto(target) => if !ids.contains(target) { return Err(format!("internal: fn #{} block {} branches to missing block {}", function.id, block.id, target)); },
            CfgTerminator::Branch(condition, yes, no) => {
                if expr_type(condition) != Type::Bool {
                    return Err(format!("internal: fn #{} block {} has a non-boolean branch condition", function.id, block.id));
                }
                if !ids.contains(yes) || !ids.contains(no) {
                    return Err(format!("internal: fn #{} block {} branches to missing blocks", function.id, block.id));
                }
            }
            CfgTerminator::Switch(_, _, targets) => for t in targets { if !ids.contains(t) { return Err(format!("internal: fn #{} switch target missing", function.id)); } },
            CfgTerminator::Return(value) if expr_type(value) != function.returns => {
                return Err(format!(
                    "internal: fn #{} block {} returns {}, expected {}",
                    function.id, block.id, expr_type(value).name(), function.returns.name(),
                ));
            }
            CfgTerminator::Exit(code) if expr_type(code) != Type::Int => {
                return Err(format!("internal: fn #{} block {} exits with a non-i64 value", function.id, block.id));
            }
            CfgTerminator::Return(_) | CfgTerminator::Exit(_) | CfgTerminator::Unreachable => {}
        }
    }
    Ok(())
}

fn print_function(function: &MirFunction, out: &mut String) {
    let name = if function.is_main { "main".to_owned() } else { format!("#{}", function.id) };
    let params = function.params.iter().map(|(id, t)| format!("v{}: {}", id, t.name())).collect::<Vec<_>>().join(", ");
    out.push_str(&format!("fn {}({}) -> {} ({} blocks)\n", name, params, function.returns.name(), function.blocks.len()));
    for block in &function.blocks {
        out.push_str(&format!("  block {}:\n", block.id));
        for binding in &block.bindings {
            let mut text = String::new();
            print_place(&binding.target, &mut text);
            text.push_str(" = ");
            print_rvalue(&binding.rvalue, &mut text);
            out.push_str(&format!("    {}\n", text));
        }
        let tail = match &block.terminator {
            CfgTerminator::Goto(target) => format!("goto block {}", target),
            CfgTerminator::Branch(cond, a, b) => format!("branch {} -> then block {} else block {}", mir_text(cond), a, b),
            CfgTerminator::Switch(subject, _, targets) => format!("switch v{} -> {}", subject, targets.iter().map(|t| format!("block {}", t)).collect::<Vec<_>>().join(", ")),
            CfgTerminator::Return(expr) => format!("return {}", mir_text(expr)),
            CfgTerminator::Exit(code) => format!("exit {}", mir_text(code)),
            CfgTerminator::Unreachable if function.is_main => "implicit exit (main)".to_owned(),
            CfgTerminator::Unreachable => "unreachable".to_owned(),
        };
        out.push_str(&format!("    {}\n", tail));
    }
}

fn mir_text(expr: &Expr) -> String {
    crate::frontend::tree::expression_text(expr)
}

fn print_place(place: &MirPlace, out: &mut String) {
    match place {
        MirPlace::Local(id, ty) => out.push_str(&format!("v{} ({})", id, ty.name())),
        MirPlace::Temp(id, ty) => out.push_str(&format!("t{} ({})", id, ty.name())),
        MirPlace::Field(base, fname, ty) => {
            print_place(base, out);
            out.push_str(&format!(".{} ({})", fname, ty.name()));
        }
    }
}

fn print_operand(operand: &MirOperand, out: &mut String) {
    match operand {
        MirOperand::Place(place) => print_place(place, out),
        MirOperand::Int(n) => out.push_str(&n.to_string()),
        // Debug formatting keeps the decimal point: `4.0` prints as `4.0`,
        // not `4`, which would read as an integer operand.
        MirOperand::Float(x) => out.push_str(&format!("{x:?}")),
        MirOperand::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        MirOperand::Borrow { slot, take_address } => {
            let operation = if *take_address { "address_of" } else { "forward_borrow" };
            out.push_str(&format!("{operation}(v{slot})"));
        }
        MirOperand::FnPtr(id) => out.push_str(&format!("fnptr#{id}")),
        MirOperand::Str(text, line, col) => out.push_str(&format!("{:?} @{}:{}", text, line, col)),
    }
}

fn print_rvalue(rvalue: &MirRvalue, out: &mut String) {
    match rvalue {
        MirRvalue::Use(operand) => print_operand(operand, out),
        MirRvalue::ShortCircuit(lhs, op, rhs) => {
            print_operand(lhs, out);
            out.push_str(&format!(" {} lazy {{ {} }} -> bool", tree::operator_text(op), mir_text(rhs)));
        }
        MirRvalue::Binary(a, op, b, ty) => {
            print_operand(a, out);
            out.push(' ');
            out.push_str(crate::frontend::tree::operator_text(op));
            out.push(' ');
            print_operand(b, out);
            out.push_str(&format!(" -> {}", ty.name()));
        }
        MirRvalue::Call(id, args, ret) => {
            out.push_str(&format!("fn#{}(", id));
            print_operands(args, out);
            out.push_str(&format!(") -> {}", ret.name()));
        }
        MirRvalue::FnCall(id, args, ret) => {
            out.push_str(&format!("v{}(", id));
            print_operands(args, out);
            out.push_str(&format!(") -> {}", ret.name()));
        }
        MirRvalue::ClosureLit(id) => {
            out.push_str(&format!("closure#{}", id));
        }
        MirRvalue::Intrinsic(name, args, ret) => {
            out.push_str(&format!("{}(", name));
            print_operands(args, out);
            out.push_str(&format!(") -> {}", ret.name()));
        }
        MirRvalue::Aggregate(sid, values) => {
            out.push_str(&format!("struct #{} {{ ", sid));
            for (index, value) in values.iter().enumerate() {
                if index > 0 { out.push_str(", "); }
                print_rvalue(value, out);
            }
            out.push_str(" }");
        }
        MirRvalue::AdtAggregate(name, eid, vidx, values) => {
            out.push_str(&format!("{}#{}::variant#{}(", name, eid, vidx));
            for (index, value) in values.iter().enumerate() {
                if index > 0 { out.push_str(", "); }
                print_rvalue(value, out);
            }
            out.push(')');
        }
        MirRvalue::VecNew(values, ty) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 { out.push_str(", "); }
                print_rvalue(value, out);
            }
            out.push_str(&format!("] ({})", ty.name()));
        }
    }
}

fn print_operands(args: &[MirOperand], out: &mut String) {
    for (index, arg) in args.iter().enumerate() {
        if index > 0 { out.push_str(", "); }
        print_operand(arg, out);
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn rejects_a_call_result_with_the_wrong_destination_type() {
        let mut function = MirFunction {
            id: 0,
            is_main: false,
            params: vec![],
            returns: Type::Int,
            blocks: vec![MirBlock {
                id: 0,
                statements: vec![],
                bindings: vec![MirBinding {
                    target: MirPlace::Temp(0, Type::Int),
                    rvalue: MirRvalue::Call(1, vec![], Type::Bool),
                }],
                terminator: CfgTerminator::Return(Expr::Number(0)),
            }],
        };
        let error = validate(&function).unwrap_err();
        assert!(error.contains("call result of type bool to a place of type i64"), "{error}");
        function.blocks[0].bindings[0].target = MirPlace::Temp(0, Type::Bool);
        assert!(validate(&function).is_ok());
    }

    #[test]
    fn rejects_a_temporary_used_before_its_definition() {
        let mut function = MirFunction {
            id: 0,
            is_main: false,
            params: vec![],
            returns: Type::Int,
            blocks: vec![MirBlock {
                id: 0,
                statements: vec![],
                bindings: vec![
                    MirBinding {
                        target: MirPlace::Temp(1, Type::Int),
                        rvalue: MirRvalue::Use(MirOperand::Place(MirPlace::Temp(0, Type::Int))),
                    },
                    MirBinding {
                        target: MirPlace::Temp(0, Type::Int),
                        rvalue: MirRvalue::Use(MirOperand::Int(42)),
                    },
                ],
                terminator: CfgTerminator::Return(Expr::Number(0)),
            }],
        };
        let error = validate(&function).unwrap_err();
        assert!(error.contains("uses undefined temporary t0"), "{error}");
        function.blocks[0].bindings.swap(0, 1);
        assert!(validate(&function).is_ok());
    }

    #[test]
    fn rejects_terminators_with_wrong_operand_types() {
        let mut function = MirFunction {
            id: 0,
            is_main: false,
            params: vec![],
            returns: Type::Int,
            blocks: vec![MirBlock {
                id: 0,
                statements: vec![],
                bindings: vec![],
                terminator: CfgTerminator::Return(Expr::BoolLit(false)),
            }],
        };
        assert!(validate(&function).unwrap_err().contains("returns bool, expected i64"));
        function.blocks[0].terminator = CfgTerminator::Exit(Expr::BoolLit(false));
        assert!(validate(&function).unwrap_err().contains("exits with a non-i64 value"));
        function.blocks[0].terminator = CfgTerminator::Exit(Expr::Number(0));
        assert!(validate(&function).is_ok());
    }
}
