//! The lowered statement tree, printed as text (`ae emit-tree`).
//!
//! A readable dump of exactly what the front end lowers before code
//! generation: declarations, then each function's statements with their
//! expressions. It is the first inspectable layer above the parser — the
//! stepping stone to the MIR construction of plan M2.2, and a stable fixture
//! format for tooling and tests. Every variant of the statement tree has a
//! form here; a new variant without a form fails the build.
use super::*;

pub(super) fn tree_path(path: &Path) -> Result<String, String> {
    tree_path_with_imports(path, &PackageImports::new())
}

pub(super) fn tree_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    let program = loader::load_program_with_imports(path, imports)?;
    let mut out = String::new();
    let mut printer = TreePrinter { out: &mut out, indent: 0, ctx: &program.ctx };
    printer.program(&program);
    Ok(out)
}

// One statement's text, for consumers that print trees inside other
// structures (the CFG printer quotes block contents this way).
pub(super) fn statement_text(statement: &Statement, ctx: &CompileContext) -> String {
    let mut out = String::new();
    let mut printer = TreePrinter { out: &mut out, indent: 0, ctx };
    printer.statement(statement);
    let mut text = out;
    while text.ends_with(char::is_whitespace) { text.pop(); }
    text
}

// One expression's text, same contract. The printer composes into its own
// parameter string; the holder keeps the borrow checker satisfied.
pub(super) fn expression_text(expr: &Expr, ctx: &CompileContext) -> String {
    let mut buf = String::new();
    let mut holder = String::new();
    let mut printer = TreePrinter { out: &mut holder, indent: 0, ctx };
    printer.expression(expr, &mut buf);
    buf
}

struct TreePrinter<'a> {
    out: &'a mut String,
    indent: usize,
    ctx: &'a CompileContext,
}

impl<'a> TreePrinter<'a> {
    fn line(&mut self, text: &str) {
        for _ in 0..self.indent { self.out.push_str("  "); }
        self.out.push_str(text);
        self.out.push('\n');
    }
    fn nested<T>(&mut self, f: impl FnOnce(&mut TreePrinter) -> T) -> T {
        self.indent += 1;
        let value = f(self);
        self.indent -= 1;
        value
    }

    fn program(&mut self, program: &Program) {
        for (sid, def) in program.structs.iter().enumerate() {
            let fields = def.fields.iter().map(|(n, t)| format!("{}: {}", n, t.name(self.ctx))).collect::<Vec<_>>().join(", ");
            let visibility = if def.public { "pub " } else { "" };
            self.line(&format!("{}struct #{} {} {{ {} }}", visibility, sid, def.name, fields));
        }
        for (eid, def) in program.enums.iter().enumerate() {
            let variants = def.variants.iter().map(|(n, fields)| {
                let payloads = fields.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", ");
                if payloads.is_empty() { n.clone() } else { format!("{}({})", n, payloads) }
            }).collect::<Vec<_>>().join(", ");
            self.line(&format!("enum #{} {} {{ {} }}", eid, def.name, variants));
        }
        for file in &program.files {
            for function in file {
                self.function(function);
            }
        }
    }

    fn function(&mut self, function: &Function) {
        let params = function.params.iter().map(|(id, t)| format!("v{}: {}", id, t.name(self.ctx))).collect::<Vec<_>>().join(", ");
        let name = if function.is_main { "main" } else { &format!("#{}", function.id) };
        self.line(&format!("fn {}({}) -> {} {{", name, params, function.returns.name(self.ctx)));
        self.nested(|this| this.statements(&function.body));
        self.line("}");
    }

    fn statements(&mut self, statements: &[Statement]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::AtomicStore(handle, value) => {
                let a = self.expression_text(handle);
                let v = self.expression_text(value);
                self.line(&format!("cell_store({}, {})", a, v));
            }
            Statement::SharedSet(handle, value, _) => {
                let h = self.expression_text(handle);
                let v = self.expression_text(value);
                self.line(&format!("shared_set({}, {})", h, v));
            }
            Statement::ChanClose(handle) => {
                let h = self.expression_text(handle);
                self.line(&format!("chan_close({})", h));
            }
            Statement::TaskSpawn(ex, slots, callable, ..) => {
                let e = self.expression_text(ex);
                let c = self.expression_text(callable);
                let caps: Vec<String> = slots.iter().map(|s| self.expression_text(s)).collect();
                if caps.is_empty() {
                    self.line(&format!("task_spawn({}, {})", e, c));
                } else {
                    self.line(&format!("task_spawn({}, {}, [{}])", e, c, caps.join(", ")));
                }
            }
            Statement::TaskYield => {
                self.line("task_yield()");
            }
            Statement::ScopeSpawn(sc, slots, callable, ..) => {
                let s = self.expression_text(sc);
                let c = self.expression_text(callable);
                let caps: Vec<String> = slots.iter().map(|x| self.expression_text(x)).collect();
                self.line(&format!("scope_spawn({}, {}, [{}])", s, c, caps.join(", ")));
            }
            Statement::PoolSpawn(p, slots, callable, ..) => {
                let s = self.expression_text(p);
                let c = self.expression_text(callable);
                let caps: Vec<String> = slots.iter().map(|x| self.expression_text(x)).collect();
                self.line(&format!("pool_spawn({}, {}, [{}])", s, c, caps.join(", ")));
            }
            Statement::Return(expr, frees) => {
                let value = self.expression_text(expr);
                let drops = self.frees_text(frees);
                self.line(&format!("return {}{}", value, drops));
            }
            Statement::Declare(id, expr) => {
                let ty = expr_type_name(expr);
                let value = self.expression_text(expr);
                self.line(&format!("v{}: {} = {}", id, ty, value));
            }
            Statement::Array(id, values, elem_ty) => {
                let items = self.expressions_text(values);
                self.line(&format!("v{}: &[{}; {}] = [{}]", id, elem_ty.name(self.ctx), values.len(), items));
            }
            Statement::Assign(id, expr, releases) => {
                let value = self.expression_text(expr);
                let drops = if *releases { " (releases old)" } else { "" };
                self.line(&format!("v{} = {}{}", id, value, drops));
            }
            Statement::Store(id, len, index, value, ..) => {
                let index_text = self.expression_text(index);
                let value_text = self.expression_text(value);
                self.line(&format!("v{}[{}] = {} (len {})", id, index_text, value_text, len));
            }
            Statement::VecPush(id, value, line, col) => {
                let value_text = self.expression_text(value);
                self.line(&format!("push v{} <- {} @ {}:{}", id, value_text, line, col));
            }
            Statement::StrVecPush(id, value, line, col, is_view) => {
                let value_text = self.expression_text(value);
                self.line(&format!("push v{} <- {}{} @ {}:{}", id, if *is_view { "view " } else { "" }, value_text, line, col));
            }
            Statement::VecFPush(id, value, line, col) | Statement::GVecPush(id, value, _, line, col) => {
                let value_text = self.expression_text(value);
                self.line(&format!("push v{} <- {} @ {}:{}", id, value_text, line, col));
            }
            Statement::VecPop(id, line, col) | Statement::StrVecPop(id, line, col) | Statement::VecFPop(id, line, col) | Statement::GVecPop(id, _, line, col) => {
                self.line(&format!("pop v{} @ {}:{}", id, line, col));
            }
            Statement::VecStore(id, index, value, line, col) | Statement::StrVecStore(id, index, value, line, col, _) | Statement::VecFStore(id, index, value, line, col) | Statement::GVecStore(id, index, value, _, line, col) => {
                let index_text = self.expression_text(index);
                let value_text = self.expression_text(value);
                self.line(&format!("v{}[{}] = {} @ {}:{}", id, index_text, value_text, line, col));
            }
            Statement::FieldStore(id, fname, fty, value, base_ty) => {
                let value_text = self.expression_text(value);
                let access = if matches!(base_ty, Type::StructMut(_)) { "->" } else { "." };
                self.line(&format!("v{}{}{} = {} (field {})", id, access, fname, value_text, fty.name(self.ctx)));
            }
            Statement::Sort(id, ascending, line, col) => {
                self.line(&format!("sort v{} {} @ {}:{}", id, if *ascending { "asc" } else { "desc" }, line, col));
            }
            Statement::Reverse(id, flag, line, col) => {
                self.line(&format!("reverse v{} (flag {}) @ {}:{}", id, flag, line, col));
            }
            Statement::Print(printed) => {
                let text = match printed {
                    Printed::Expr(e) => self.expression_text(e),
                    Printed::Text(t) => format!("{:?}", t),
                };
                self.line(&format!("print {}", text));
            }
            Statement::Assert(cond, line, col) => {
                let cond_text = self.expression_text(cond);
                self.line(&format!("assert {} @ {}:{}", cond_text, line, col));
            }
            Statement::If(cond, yes, no) => {
                let cond_text = self.expression_text(cond);
                self.line(&format!("if {} {{", cond_text));
                self.nested(|this| this.statements(yes));
                self.line("} else {");
                self.nested(|this| this.statements(no));
                self.line("}");
            }
            Statement::While(cond, body) => {
                let cond_text = self.expression_text(cond);
                self.line(&format!("while {} {{", cond_text));
                self.nested(|this| this.statements(body));
                self.line("}");
            }
            Statement::For(var, container, body, line, col, label) => {
                self.line(&format!("for v{} in v{} @ {}:{} (label #{}) {{", var, container, line, col, label));
                self.nested(|this| this.statements(body));
                self.line("}");
            }
            Statement::ForTokens(var, base, body, line, col, label) => {
                self.line(&format!("for v{} in tokens of v{} @ {}:{} (label #{}) {{", var, base, line, col, label));
                self.nested(|this| this.statements(body));
                self.line("}");
            }
            Statement::Match(subject, ty, arms) => {
                self.line(&format!("match v{} ({}) {{", subject, ty.name(self.ctx)));
                self.nested(|this| {
                    for arm in arms {
                        let bindings = arm.bindings.iter().map(|(id, t, idx)| format!("v{}: {}#{}", id, t.name(this.ctx), idx)).collect::<Vec<_>>().join(", ");
                        this.line(&format!("variant #{} ({}) => {{", arm.variant, bindings));
                        this.nested(|inner| inner.statements(&arm.body));
                        this.line("}");
                    }
                });
                self.line("}");
            }
            Statement::MapPut(place, key, value, line, col) => {
                let place_text = self.map_place_text(place);
                let key_text = self.expression_text(key);
                let value_text = self.expression_text(value);
                self.line(&format!("{}[{}] = {} @ {}:{}", place_text, key_text, value_text, line, col));
            }
            // CFG-only (M2.3): a match arm's payload binding, printed where
            // the lowering placed it — the top of the arm's block.
            Statement::ResetTokens(cursor, iteration) => self.line(&format!("v{iteration} = restart token cursor v{cursor}")),
            Statement::AdvanceTokens(_, cursor, element, advanced) => {
                self.line(&format!("v{advanced}: bool = advance cursor v{cursor} into v{element}"));
            }
            Statement::TakePropagationPayload(subject, ty, _, binding) => {
                let destination = match binding {
                    Some((id, ty)) => format!(" into v{id}: {}", ty.name(self.ctx)),
                    None => " and discard it".to_owned(),
                };
                self.line(&format!("take success payload of v{subject}: {}{destination}", ty.name(self.ctx)));
            }
            Statement::BindPayload(bid, subject, variant, index, ty) => {
                self.line(&format!("bind v{bid} = payload #{index} of v{subject}::#{variant} ({})", ty.name(self.ctx)));
            }
            Statement::Propagate(expr, ty, ok_types, binding, frees) => {
                let expr_text = self.expression_text(expr);
                let bound = match binding {
                    Some((id, t)) => format!(" bind v{}: {}", id, t.name(self.ctx)),
                    None => String::new(),
                };
                let oks = ok_types.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", ");
                let drops = self.frees_text(frees);
                self.line(&format!("propagate {} ({}) -> {}{}{}", expr_text, oks, ty.name(self.ctx), bound, drops));
            }
            Statement::Break => self.line("break"),
            Statement::Continue(depth) => self.line(&format!("continue (depth {})", depth.unwrap_or(0))),
            Statement::Exit(code, frees) => {
                let code_text = self.expression_text(code);
                let drops = self.frees_text(frees);
                self.line(&format!("exit {}{}", code_text, drops));
            }
            Statement::Frees(owned) => {
                // The lowering can attach an empty release list on paths with
                // nothing owned; printing it would be noise. The list is the
                // content itself — `frees_text` is for annotations.
                if owned.is_empty() { return; }
                let list = owned.iter().map(|(id, t)| format!("v{}: {}", id, t.name(self.ctx))).collect::<Vec<_>>().join(", ");
                self.line(&format!("release {}", list));
            }
            Statement::Unsafe(inner) => {
                self.line("unsafe {");
                self.indent += 1;
                for statement in inner { self.statement(statement); }
                self.indent -= 1;
                self.line("}");
            }
            Statement::PtrStore(id, value, ..) => {
                let value_text = self.expression_text(value);
                self.line(&format!("*v{id} = {}", value_text));
            }
        }
    }

    fn expressions_text(&mut self, values: &[Expr]) -> String {
        let pieces: Vec<String> = values.iter().map(|v| self.expression_text(v)).collect();
        pieces.join(", ")
    }

    fn frees_text(&self, frees: &[(usize, Type)]) -> String {
        if frees.is_empty() { return String::new(); }
        let list = frees.iter().map(|(id, t)| format!("v{}: {}", id, t.name(self.ctx))).collect::<Vec<_>>().join(", ");
        format!(" (release {})", list)
    }

    fn map_place_text(&self, place: &MapPlace) -> String {
        match place {
            MapPlace::Var(id, kind) => format!("v{}[{}]", id, if *kind == MapKind::Float { "f64" } else { "i64" }),
            MapPlace::Field(id, fname, kind) => format!("v{}.{}[{}]", id, fname, if *kind == MapKind::Float { "f64" } else { "i64" }),
        }
    }

    fn expression_text(&mut self, expr: &Expr) -> String {
        let mut text = String::new();
        self.expression(expr, &mut text);
        text
    }

    fn expression(&mut self, expr: &Expr, out: &mut String) {
        match expr {
            Expr::Number(n) => out.push_str(&n.to_string()),
            Expr::FloatLit(x) => out.push_str(&x.to_string()),
            Expr::BoolLit(b) => out.push_str(if *b { "true" } else { "false" }),
            Expr::CharLit(c) => out.push_str(&format!("{c:?}")),
            Expr::Unit => out.push_str("()"),
            Expr::StrLit(text, line, col) => out.push_str(&format!("{:?} @ {}:{}", text, line, col)),
            Expr::Variable(id, ty, line, col) => out.push_str(&format!("v{} ({}) @ {}:{}", id, ty.name(self.ctx), line, col)),
            Expr::Field(base, fname, fty) => {
                self.expression(base, out);
                out.push_str(&format!(".{} ({})", fname, fty.name(self.ctx)));
            }
            Expr::Call(id, args, ret) => {
                out.push_str(&format!("fn#{}(", id));
                self.arguments(args, out);
                out.push_str(&format!(") -> {}", ret.name(self.ctx)));
            }
            Expr::FnCall(id, args, ret, _) => {
                out.push_str(&format!("v{}(", id));
                self.arguments(args, out);
                out.push_str(&format!(") -> {}", ret.name(self.ctx)));
            }
            Expr::FnExprCall(callee, args, ret, _) => {
                out.push_str(&format!("({})(", self.expression_text(callee)));
                self.arguments(args, out);
                out.push_str(&format!(") -> {}", ret.name(self.ctx)));
            }
            // M2.12 slice 2: the raw-pointer shapes.
            Expr::BorrowAs(operand, ..) => {
                out.push('&');
                out.push_str(&self.expression_text(operand));
            }
            Expr::DerefPtr(pointer, _, _) => {
                out.push('*');
                out.push_str(&self.expression_text(pointer));
            }
            Expr::PtrCast(operand, mutable, pointee_id, _, _) => {
                out.push('(');
                out.push_str(if *mutable { "*mut " } else { "*const " });
                out.push_str(&self.ctx.ptr_pointee(*pointee_id).name(self.ctx));
                out.push_str(")(");
                out.push_str(&self.expression_text(operand));
                out.push(')');
            }
            Expr::PtrWrite(pointer, value, _, _, _) => {
                out.push_str("ptr_write(");
                out.push_str(&self.expression_text(pointer));
                out.push_str(", ");
                out.push_str(&self.expression_text(value));
                out.push(')');
            }
            Expr::PtrDropInPlace(pointer, _, _, _) => {
                out.push_str("ptr_drop_in_place(");
                out.push_str(&self.expression_text(pointer));
                out.push(')');
            }
            Expr::PtrOffset(pointer, count, bytewise, _, _) => {
                let name = if *bytewise { "byte_offset" } else { "offset" };
                out.push_str(name);
                out.push('(');
                out.push_str(&self.expression_text(pointer));
                out.push_str(", ");
                out.push_str(&self.expression_text(count));
                out.push(')');
            }
            Expr::ClosureLit(cid) => out.push_str(&format!("closure#{}", cid)),
            Expr::FnRef(fn_id, _) => out.push_str(&format!("fn#{}", fn_id)),
            Expr::AtomicNew(value) => out.push_str(&format!("cell_new({})", self.expression_text(value))),
            Expr::AtomicLoad(handle) => out.push_str(&format!("cell_load({})", self.expression_text(handle))),
            Expr::SharedNew(value, _) => out.push_str(&format!("shared_new({})", self.expression_text(value))),
            Expr::SharedGet(handle, _) => out.push_str(&format!("shared_get({})", self.expression_text(handle))),
            Expr::MutexNew => out.push_str("mutex_new()"),
            Expr::MutexLock(handle) => out.push_str(&format!("mutex_lock({})", self.expression_text(handle))),
            Expr::ChanNew(cap) => out.push_str(&format!("chan_new({})", self.expression_text(cap))),
            Expr::ChanSend(ch, val) => out.push_str(&format!("chan_send({}, {})", self.expression_text(ch), self.expression_text(val))),
            Expr::ChanTrySend(ch, val) => out.push_str(&format!("chan_try_send({}, {})", self.expression_text(ch), self.expression_text(val))),
            Expr::ChanRecv(ch) => out.push_str(&format!("chan_recv({})", self.expression_text(ch))),
            Expr::ChanSelect(c1, c2) => out.push_str(&format!("chan_select({}, {})", self.expression_text(c1), self.expression_text(c2))),
            Expr::ExecNew => out.push_str("exec_new()"),
            Expr::ExecRun(ex) => out.push_str(&format!("exec_run({})", self.expression_text(ex))),
            Expr::ScopeNew => out.push_str("scope_new()"),
            Expr::PoolNew(n) => out.push_str(&format!("pool_new({})", self.expression_text(n))),
            Expr::PoolJoin(p) => out.push_str(&format!("pool_join({})", self.expression_text(p))),
            Expr::AtomicAdd(handle, value) => out.push_str(&format!("cell_add({}, {})", self.expression_text(handle), self.expression_text(value))),
            Expr::AtomicCas(handle, expect, new) => out.push_str(&format!("cell_cas({}, {}, {})", self.expression_text(handle), self.expression_text(expect), self.expression_text(new))),
            // The handle and the capture slots print like any expression; the
            // shape mirrors the surface form the programmer wrote.
            Expr::Spawn(slots, callable, ..) => {
                out.push_str("spawn(");
                self.expression(callable, out);
                for slot in slots {
                    out.push_str(", ");
                    self.expression(slot, out);
                }
                out.push(')');
            }
            Expr::Wait(handle, ..) => {
                out.push_str("wait(");
                self.expression(handle, out);
                out.push(')');
            }
            Expr::Binary(a, op, b, line, col, ty) => {
                self.expression(a, out);
                out.push_str(&format!(" {} ", operator_text(op)));
                self.expression(b, out);
                out.push_str(&format!(" ({} @ {}:{})", ty.name(self.ctx), line, col));
            }
            Expr::Neg(a, line, col) => {
                out.push('-');
                self.expression(a, out);
                out.push_str(&format!(" @ {}:{}", line, col));
            }
            Expr::Not(a) => {
                out.push('!');
                self.expression(a, out);
            }
            Expr::StrEq(a, b, negated) => {
                self.expression(a, out);
                out.push_str(if *negated { " !=s " } else { " ==s " });
                self.expression(b, out);
            }
            Expr::Len(a, line, col) => {
                out.push_str("len(");
                self.expression(a, out);
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::StructLit(sid, values) => {
                out.push_str(&format!("struct #{} {{ ", sid));
                for (index, value) in values.iter().enumerate() {
                    if index > 0 { out.push_str(", "); }
                    self.expression(value, out);
                }
                out.push_str(" }");
            }
            Expr::EnumLit(name, eid, vidx, values) => {
                out.push_str(&format!("{}#{}::variant#{}(", name, eid, vidx));
                for (index, value) in values.iter().enumerate() {
                    if index > 0 { out.push_str(", "); }
                    self.expression(value, out);
                }
                out.push(')');
            }
            Expr::VecLit(values, ty, line, col) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 { out.push_str(", "); }
                    self.expression(value, out);
                }
                out.push_str(&format!("] ({}) @ {}:{}", ty.name(self.ctx), line, col));
            }
            Expr::Index(id, len, index, line, col, _) => {
                out.push_str(&format!("v{}[", id));
                self.expression(index, out);
                out.push_str(&format!("] (len {} @ {}:{})", len, line, col));
            }
            Expr::VecIndex(id, index, line, col, borrowed, elem) => {
                out.push_str(&format!("v{}{}[", id, if *borrowed { "&" } else { "" }));
                self.expression(index, out);
                out.push_str(&format!("] ({}) @ {}:{}", elem.name(self.ctx), line, col));
            }
            Expr::VecIndexOf(base, index, line, col, elem) => {
                self.expression(base, out);
                out.push('[');
                self.expression(index, out);
                out.push_str(&format!("] ({}) @ {}:{}", elem.name(self.ctx), line, col));
            }
            Expr::StrVecIndex(id, index, line, col, borrowed) => {
                out.push_str(&format!("v{}{}[", id, if *borrowed { "&" } else { "" }));
                self.expression(index, out);
                out.push_str(&format!("] (view) @ {}:{}", line, col));
            }
            Expr::SliceView(id, field, start, end, line, col) => {
                out.push_str(&format!("slice_view(v{}", id));
                if let Some(fname) = field { out.push_str(&format!(".{}", fname)); }
                out.push_str(", ");
                self.expression(start, out);
                out.push_str(", ");
                self.expression(end, out);
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::Tokens(id, field, sep, line, col) => {
                out.push_str(&format!("tokens(v{}", id));
                if let Some(fname) = field { out.push_str(&format!(".{}", fname)); }
                out.push_str(", ");
                self.expression(sep, out);
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::MapNew(line, col, kind) => out.push_str(&format!("map_new({}) @ {}:{}", if *kind == MapKind::Float { "f64" } else { "i64" }, line, col)),
            Expr::MapGet(place, key, ret, line, col) => {
                out.push_str(&format!("map_get({}, ", self.map_place_text(place)));
                self.expression(key, out);
                out.push_str(&format!(") -> {} @ {}:{}", ret.name(self.ctx), line, col));
            }
            Expr::MapHas(place, key, line, col) | Expr::MapRemove(place, key, line, col) => {
                out.push_str(if matches!(expr, Expr::MapHas(..)) { "map_has(" } else { "map_remove(" });
                out.push_str(&self.map_place_text(place));
                out.push_str(", ");
                self.expression(key, out);
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::MapInc(place, key, delta, line, col) => {
                out.push_str(&format!("map_inc({}, ", self.map_place_text(place)));
                self.expression(key, out);
                out.push_str(", ");
                self.expression(delta, out);
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::MapKeys(place, line, col) => out.push_str(&format!("keys({}) @ {}:{}", self.map_place_text(place), line, col)),
            Expr::MathCall(index, args, line, col) => {
                let (name, _, _) = MATH_FNS[*index as usize];
                out.push_str(&format!("{}(", name));
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 { out.push_str(", "); }
                    self.expression(arg, out);
                }
                out.push_str(&format!(") @ {}:{}", line, col));
            }
            Expr::IntMethod(kind, recv, arg, ty, line, col) => {
                out.push_str(&format!("{}(", kind.mir_name()));
                self.expression(recv, out);
                if let Some(arg) = arg {
                    out.push_str(", ");
                    self.expression(arg, out);
                }
                out.push_str(&format!(") -> {} @ {}:{}", ty.name(self.ctx), line, col));
            }
            other => out.push_str(&format!("<expr {:?}>", std::mem::discriminant(other))),
        }
    }

    fn arguments(&mut self, args: &[Argument], out: &mut String) {
        for (index, arg) in args.iter().enumerate() {
            if index > 0 { out.push_str(", "); }
            match arg {
                Argument::Value(e) => self.expression(e, out),
                Argument::View(e) => {
                    out.push_str("view ");
                    self.expression(e, out);
                }
                Argument::Borrow(id, takes_address) => out.push_str(&format!("{}v{}", if *takes_address { "&" } else { "" }, id)),
                Argument::FnPtr(id) => out.push_str(&format!("fnptr#{id}")),
            }
        }
    }
}

pub(super) fn operator_text(op: &Kind) -> &'static str {
    match op {
        Kind::Plus => "+", Kind::Minus => "-", Kind::Star => "*", Kind::Slash => "/", Kind::Percent => "%",
        Kind::EqEq => "==", Kind::NotEq => "!=", Kind::Less => "<", Kind::LessEq => "<=",
        Kind::Greater => ">", Kind::GreaterEq => ">=", Kind::And => "&&", Kind::Or => "||",
        _ => "?",
    }
}

fn expr_type_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Number(..) | Expr::Binary(..) => "i64",
        Expr::FloatLit(..) | Expr::ToF64(..) => "f64",
        Expr::CharLit(..) => "char",
        Expr::Unit => "()",
        Expr::BoolLit(..) | Expr::StrEq(..) | Expr::Contains(..) | Expr::StartsWith(..) | Expr::EndsWith(..) => "bool",
        _ => "owned",
    }
}
