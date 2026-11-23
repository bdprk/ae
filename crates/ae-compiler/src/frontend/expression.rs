//! Expression parsing and type checking.
use super::*;
use super::closures::SpawnedThread;
use super::parser::moved_value_hint;

impl Parser<'_> {
    // `value.method(...)` with no such method. Naming what the type does have
    // turns a typo into a one-line fix; a type with no impl at all gets the
    // declaration form instead, since there is nothing to list.
    // `Type::name(args)` reaches an associated function — one declared without
    // a receiver. It is the same call a plain function would be, with the type
    // name standing in for the namespace the impl block created.
    fn associated_call(&mut self, type_name: &str, offset: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::ColonColon)?;
        let method = self.identifier()?;
        if let Some(&sid) = self.struct_names.get(type_name) {
            let def = &self.structs[sid];
            self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
        } else if let Some(&eid) = self.enum_names.get(type_name) {
            let def = &self.enums[eid];
            self.require_public("enum", def.name, &def.file, def.public)?;
        } else {
            return Err(self.error_at(offset, &format!("'{type_name}' is not a type with an impl block")));
        }
        let key = format!("{type_name}#{method}");
        let Some(def) = self.generics.inherent.get(&key).cloned() else {
            return Err(self.no_such_method(type_name, &method, "associated function"));
        };
        if def.receiver.is_some() {
            return Err(self.error_at(offset, &format!("'{type_name}::{method}' takes a receiver; call it as 'value.{method}(...)' instead")));
        }
        if !def.signature.public {
            return Err(self.error_at(offset, &format!("associated function '{type_name}::{method}' is private")));
        }
        let params = def.signature.params.clone();
        let args = self.call_arguments(&format!("{type_name}::{method}"), &params)?;
        Ok((Expr::Call(def.signature.id, args, def.signature.ret), def.signature.ret))
    }

    // A compile-time integer constant as an expression: the layout queries
    // evaluate in the parser and reach every later stage as plain numbers.
    fn value_number(&self, n: i64) -> (Expr, Type) {
        (Expr::Number(n), Type::Int)
    }

    fn no_such_method(&self, type_name: &str, method: &str, form: &str) -> ae_span::Diagnostic {
        let prefix = format!("{type_name}#");
        let mut names: Vec<String> = self.generics.inherent.keys()
            .filter_map(|key| key.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        // A generic struct keeps its methods in its impl template, keyed by
        // the declaration name; list them by their own names (ADR-0187).
        if let Some((gid, _)) = self.struct_origin_of(type_name) {
            let def_name = self.generics.struct_defs[gid].name.clone();
            if let Some(gimpl) = self.generics.generic_impls.get(&def_name) {
                names.extend(gimpl.methods.iter().map(|m| m.name.clone()));
            }
        }
        if names.is_empty() {
            return self.fail(&format!("type '{type_name}' has no {form}s; declare one in 'impl {type_name} {{ ... }}'"));
        }
        names.sort();
        names.dedup();
        self.fail(&format!("type '{type_name}' has no {form} '{method}'; it has {}", names.join(", ")))
    }

    // The generic declaration behind a type name, when the name is an
    // instantiation. Accepts the display spelling (Pair[string, i64]) as
    // well as the declaration name (Pair).
    fn struct_origin_of(&self, type_name: &str) -> Option<(usize, Vec<Type>)> {
        if let Some(&(gid, ref args)) = self.generics.struct_origin.values().find(|(gid, _)| {
            self.generics.struct_defs[*gid].name == type_name
        }) {
            return Some((gid, args.clone()));
        }
        for (sid, origin) in &self.generics.struct_origin {
            if self.structs[*sid].name == type_name {
                return Some(origin.clone());
            }
        }
        None
    }

    // The four atomic operations parse in helpers, not inline in the
    // dispatch: each arm's locals would otherwise grow expression_inner's
    // frame, and the deep-import probe pins a 512KB stack across that
    // recursion (spec/concurrency.md §8).
    #[inline(never)]
    fn cell_new_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (seed, seed_ty) = self.expression(0, false)?;
        if seed_ty != Type::Int { return Err(self.fail("cell_new expects an i64 seed")); }
        self.take(Kind::RParen)?;
        Ok((Expr::AtomicNew(Box::new(seed)), Type::Atomic))
    }
    #[inline(never)]
    fn cell_load_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, false)?;
        if handle_ty != Type::Atomic { return Err(self.fail("cell_load expects an atomic handle")); }
        self.take(Kind::RParen)?;
        Ok((Expr::AtomicLoad(Box::new(handle)), Type::Int))
    }
    #[inline(never)]
    fn cell_add_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, false)?;
        if handle_ty != Type::Atomic { return Err(self.fail("cell_add expects an atomic handle")); }
        self.take(Kind::Comma)?;
        let (value, value_ty) = self.expression(0, false)?;
        if value_ty != Type::Int { return Err(self.fail("cell_add expects an i64 delta")); }
        self.take(Kind::RParen)?;
        Ok((Expr::AtomicAdd(Box::new(handle), Box::new(value)), Type::Int))
    }
    #[inline(never)]
    fn cell_cas_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, false)?;
        if handle_ty != Type::Atomic { return Err(self.fail("cell_cas expects an atomic handle")); }
        self.take(Kind::Comma)?;
        let (expect, expect_ty) = self.expression(0, false)?;
        if expect_ty != Type::Int { return Err(self.fail("cell_cas expects an i64 expected value")); }
        self.take(Kind::Comma)?;
        let (new, new_ty) = self.expression(0, false)?;
        if new_ty != Type::Int { return Err(self.fail("cell_cas expects an i64 new value")); }
        self.take(Kind::RParen)?;
        Ok((Expr::AtomicCas(Box::new(handle), Box::new(expect), Box::new(new)), Type::Bool))
    }
    /// `c_size_of[T]()` / `c_align_of[T]()` / `c_offset_of[T]("field")` — cursor
    /// on the `[`. Compile-time layout constants over a `#[repr(C)]` struct
    /// (M2.12 U8), so a layout regression fails a test rather than an FFI
    /// session. Shared by the typed dispatch and the resolved body route.
    #[inline(never)]
    pub(super) fn c_layout_expr(&mut self, name: &str) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.pos += 1;
        let type_name = self.identifier()?;
        // A type parameter inside a generic body resolves through the
        // substitution table first (ADR-0209 step A): at instantiation the
        // body is re-parsed with T bound to the concrete struct, so
        // `c_size_of[T]()` must see through it. An unsubstituted unknown name
        // keeps today's error.
        let scalar = match self.subst.get(&type_name) {
            Some(ty) => ty.scalar_c_layout(),
            None => match type_name.as_str() {
                "i64" => Type::Int.scalar_c_layout(),
                "f64" => Type::F64.scalar_c_layout(),
                "bool" => Type::Bool.scalar_c_layout(),
                _ => None,
            },
        };
        if name != "c_offset_of" && let Some((size, align)) = scalar {
            self.take(Kind::RBracket)?;
            self.take(Kind::LParen)?;
            if *self.current() != Kind::RParen { return Err(self.fail(&format!("{name} takes no arguments; write {name}[Type]()"))); }
            self.pos += 1;
            return Ok(self.value_number(if name == "c_align_of" { align } else { size }));
        }
        Ok({
            let type_name = match self.subst.get(&type_name) {
                Some(Type::Struct(sid)) => self.structs[*sid].name.clone(),
                _ => type_name,
            };
            if let Some(&eid) = self.enum_names.get(&type_name) {
                if name == "c_offset_of" {
                    return Err(self.fail(&format!("'{}' is not a struct; c_offset_of names concrete structs", type_name)));
                }
                self.take(Kind::RBracket)?;
                self.take(Kind::LParen)?;
                if *self.current() != Kind::RParen {
                    return Err(self.fail(&format!("{name} takes no arguments; write {name}[Enum]()")));
                }
                self.pos += 1;
                let (size, align) = self.enums[eid].c_layout(self.structs);
                self.value_number(if name == "c_align_of" { align } else { size })
            } else {
                let Some(&sid) = self.struct_names.get(&type_name) else {
                    return Err(self.fail(&format!("'{}' is not a struct; the layout queries name concrete structs", type_name)));
                };
                if !self.structs[sid].repr_c {
                    return Err(self.fail(&format!("'{}' has no '#[repr(C)]', so its layout is the compiler's choice; the queries pin promised layouts only", type_name)));
                }
                self.take(Kind::RBracket)?;
                self.take(Kind::LParen)?;
                if name == "c_size_of" || name == "c_align_of" {
                    if *self.current() != Kind::RParen { return Err(self.fail(&format!("{name} takes no arguments; write {name}[Struct]()"))); }
                    self.pos += 1;
                    let Some((size, align, _)) = self.structs[sid].c_layout(self.structs) else {
                        return Err(self.fail("internal: a repr(C) struct without a layout"));
                    };
                    self.value_number(if name == "c_align_of" { align } else { size })
                } else {
                    let (field, field_ty) = self.expression(0, false)?;
                    if !matches!(field_ty, Type::String) { return Err(self.fail("offset_of's field is written as a string literal")); }
                    let Expr::StrLit(fname, _, _) = &field else {
                        return Err(self.fail("offset_of's field is written as a string literal"));
                    };
                    self.take(Kind::RParen)?;
                    let Some((_, _, offsets)) = self.structs[sid].c_layout(self.structs) else {
                        return Err(self.fail("internal: a repr(C) struct without a layout"));
                    };
                    let index = self.structs[sid].fields.iter().position(|(n, _)| n == fname)
                        .ok_or_else(|| self.fail(&format!("'{}' has no field '{}'", type_name, fname)))?;
                    self.value_number(offsets[index])
                }
            }
        })
    }

    /// `spawn(closure)` — cursor after the name. The thread runs the callable
    /// once with no arguments; the data travels in its owned captures
    /// (spec/concurrency.md §1–§3). Shared by the typed dispatch and the
    /// resolved body route.
    #[inline(never)]
    pub(super) fn spawn_expr(&mut self, line: usize, col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.spawn_expr_with(line, col, None, &HashMap::new())
    }

    pub(super) fn spawn_expr_with(
        &mut self,
        line: usize,
        col: usize,
        callable: Option<&ae_syntax::expression::Expression>,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let at = self.tokens[self.pos - 1].offset;
        self.take(Kind::LParen)?;
        let (callable, callable_ty) = self.spawn_callable_with(callable, slots)?;
        self.take(Kind::RParen)?;
        let mut slots = Vec::new();
        let tid = match (&callable, callable_ty) {
            (Expr::ClosureLit(cid), Type::Closure(_)) | (Expr::Variable(_, Type::Closure(cid), ..), _) => {
                let def = self.generics.closures[*cid].clone();
                if !def.params.is_empty() {
                    return Err(self.fail("the spawned closure must take no parameters; move the data it needs through its captures"));
                }
                // ADR-0215: borrow captures are accepted when the handle is
                // provably joined before the borrowed region ends — the escape
                // proof defers to the function's CFG. Until the proof lands,
                // the borrowed variables freeze like views (no write, no
                // move), which is sound on its own.
                if !def.borrow_captures.is_empty() {
                    let borrowed: Vec<usize> = def.borrow_captures.iter().map(|(outer_id, _)| *outer_id).collect();
                    let depth = self.scopes.len();
                    for outer_id in &borrowed {
                        self.borrows.push((*outer_id, depth, BorrowKind::View));
                    }
                    self.deferred_borrow_spawns.push((*cid, borrowed, at));
                }
                for (outer_id, ty) in &def.value_captures {
                    // Arrays and cursors read the caller's storage no matter
                    // how the capture is classified, and closure/pointer
                    // values have no byte layout to package; owned scalars,
                    // strings, vectors, maps, structs, and enums move.
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF | Type::Struct(_) | Type::Enum(..) | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool) {
                        return Err(self.fail("spawn moves its captures into the thread; this capture's storage cannot cross"));
                    }
                    if self.moved.contains(outer_id) {
                        return Err(moved_value_hint(self.error_at(at, "cannot spawn a closure that captures a moved variable").with_code("E0102")));
                    }
                    if self.is_borrowed(*outer_id) {
                        return Err(self.fail("cannot spawn while one of the closure's captured variables is borrowed"));
                    }
                    if ty.owned() { self.moved.insert(*outer_id); }
                    slots.push(Expr::Variable(*outer_id, *ty, 0, 0));
                }
                let tid = self.intern_thread(def.ret);
                self.ctx.record_spawned(SpawnedThread::Closure {
                    fn_id: self.ctx.closure_fn_id(*cid),
                    captures: def.value_captures.iter().map(|(_, ty)| *ty).collect(),
                    borrows: def.borrow_captures.clone(),
                    ret: def.ret,
                });
                tid
            }
            (Expr::Variable(_, Type::Fn(sig), ..), _) => {
                let (params, ret) = self.ctx.fn_sig(*sig);
                if !params.is_empty() {
                    return Err(self.fail("the spawned function value must take no parameters"));
                }
                let tid = self.intern_thread(ret);
                self.ctx.record_spawned(SpawnedThread::FnValue { sig: *sig, ret });
                tid
            }
            _ => return Err(self.fail("spawn needs a closure value; move the data the thread needs through its captures")),
        };
        Ok((Expr::Spawn(slots, Box::new(callable), tid, line, col), Type::Thread(tid)))
    }

    /// `wait(handle)` — cursor after the name. The handle is consumed: the
    /// move rules reject a second wait, and the thread's slot transfers its
    /// return value (spec/concurrency.md §1 rule 3).
    #[inline(never)]
    pub(super) fn wait_expr(&mut self, line: usize, col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, true)?;
        self.take(Kind::RParen)?;
        let tid = match handle_ty {
            Type::Thread(tid) => tid,
            _ => return Err(self.fail("wait expects a thread handle")),
        };
        let ret = self.generics.threads[tid];
        Ok((Expr::Wait(Box::new(handle), ret, line, col), ret))
    }

    #[inline(never)]
    fn shared_new_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (val, val_ty) = self.expression(0, false)?;
        if matches!(val_ty, Type::Shared(_)) {
            return Err(self.fail("shared_new cannot wrap another shared handle (no handle towers)"));
        }
        if !matches!(val_ty, Type::Int | Type::Bool | Type::F64 | Type::Atomic) {
            return Err(self.fail("shared_new in v1 expects a copy-semantics value (i64, bool, f64, or atomic)"));
        }
        self.take(Kind::RParen)?;
        let sid = self.ctx.intern_shared(val_ty);
        Ok((Expr::SharedNew(Box::new(val), sid), Type::Shared(sid)))
    }
    #[inline(never)]
    fn shared_get_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, false)?;
        let sid = match handle_ty {
            Type::Shared(sid) => sid,
            _ => return Err(self.fail("shared_get expects a shared handle")),
        };
        self.take(Kind::RParen)?;
        Ok((Expr::SharedGet(Box::new(handle), sid), self.ctx.shared_types_get(sid)))
    }
    #[inline(never)]
    fn mutex_new_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        self.take(Kind::RParen)?;
        Ok((Expr::MutexNew, Type::Mutex))
    }
    #[inline(never)]
    fn mutex_lock_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (handle, handle_ty) = self.expression(0, false)?;
        if handle_ty != Type::Mutex {
            return Err(self.fail("mutex_lock expects a mutex handle"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::MutexLock(Box::new(handle)), Type::Guard))
    }
    #[inline(never)]
    fn chan_new_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (cap, cap_ty) = self.expression(0, false)?;
        if cap_ty != Type::Int {
            return Err(self.fail("chan_new expects an i64 capacity"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ChanNew(Box::new(cap)), Type::Chan))
    }
    #[inline(never)]
    fn chan_send_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (ch, ch_ty) = self.expression(0, false)?;
        if ch_ty != Type::Chan {
            return Err(self.fail("chan_send expects a chan handle"));
        }
        self.take(Kind::Comma)?;
        let (val, val_ty) = self.expression(0, false)?;
        if val_ty != Type::Int {
            return Err(self.fail("chan_send expects an i64 value"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ChanSend(Box::new(ch), Box::new(val)), Type::Bool))
    }
    #[inline(never)]
    fn chan_recv_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (ch, ch_ty) = self.expression(0, false)?;
        if ch_ty != Type::Chan {
            return Err(self.fail("chan_recv expects a chan handle"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ChanRecv(Box::new(ch)), Type::Int))
    }

    #[inline(never)]
    fn chan_try_send_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (ch, ch_ty) = self.expression(0, false)?;
        if ch_ty != Type::Chan {
            return Err(self.fail("chan_try_send expects a chan handle"));
        }
        self.take(Kind::Comma)?;
        let (val, val_ty) = self.expression(0, false)?;
        if val_ty != Type::Int {
            return Err(self.fail("chan_try_send expects an i64 value"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ChanTrySend(Box::new(ch), Box::new(val)), Type::Bool))
    }
    #[inline(never)]
    fn chan_select_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (c1, c1_ty) = self.expression(0, false)?;
        if c1_ty != Type::Chan {
            return Err(self.fail("chan_select expects chan handles"));
        }
        self.take(Kind::Comma)?;
        let (c2, c2_ty) = self.expression(0, false)?;
        if c2_ty != Type::Chan {
            return Err(self.fail("chan_select expects chan handles"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ChanSelect(Box::new(c1), Box::new(c2)), Type::Int))
    }
    #[inline(never)]
    fn exec_new_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        self.take(Kind::RParen)?;
        Ok((Expr::ExecNew, Type::Exec))
    }
    #[inline(never)]
    fn exec_run_expr(&mut self, _line: usize, _col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        let (ex, ex_ty) = self.expression(0, false)?;
        if ex_ty != Type::Exec {
            return Err(self.fail("exec_run expects an exec handle"));
        }
        self.take(Kind::RParen)?;
        Ok((Expr::ExecRun(Box::new(ex)), Type::Int))
    }
    #[inline(never)]
    fn clock_timer_expr(&mut self, name: &str) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        match name {
            // ADR-0224 R2.2/R2.4: the fake clock, timers, and cancel tokens
            // are ae source (std/test, std/task, std/sync) over cell_*.
            "scope_new" => { self.take(Kind::RParen)?; Ok((Expr::ScopeNew, Type::TaskScope)) }
            "pool_new" => {
                let (n, ty) = self.expression(0, false)?;
                if ty != Type::Int { return Err(self.fail("pool_new expects an i64 worker count")); }
                self.take(Kind::RParen)?;
                Ok((Expr::PoolNew(Box::new(n)), Type::Pool))
            }
            "pool_join" => {
                let (p, ty) = self.expression(0, false)?;
                if ty != Type::Pool { return Err(self.fail("pool_join expects a pool handle")); }
                self.take(Kind::RParen)?;
                Ok((Expr::PoolJoin(Box::new(p)), Type::Int))
            }
            _ => unreachable!("clock_timer_expr dispatched for {name}"),
        }
    }
    #[inline(never)]
    fn concurrency_builtin_expr(&mut self, name: &str, line: usize, col: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        match name {
            "shared_new" => self.shared_new_expr(line, col),
            "shared_get" => self.shared_get_expr(line, col),
            "mutex_new" => self.mutex_new_expr(line, col),
            "mutex_lock" => self.mutex_lock_expr(line, col),
            "chan_new" => self.chan_new_expr(line, col),
            "chan_send" => self.chan_send_expr(line, col),
            "chan_try_send" => self.chan_try_send_expr(line, col),
            "chan_select" => self.chan_select_expr(line, col),
            "exec_new" => self.exec_new_expr(line, col),
            "exec_run" => self.exec_run_expr(line, col),
            "scope_new" | "pool_new" | "pool_join" => self.clock_timer_expr(name),
            _ => self.chan_recv_expr(line, col),
        }
    }

    // One interned shape per distinct return type: wait's result type comes
    // from the handle (spec/concurrency.md §2).
    pub(super) fn intern_thread(&mut self, ret: Type) -> usize {
        self.generics.threads.iter().position(|existing| *existing == ret)
            .unwrap_or_else(|| { self.generics.threads.push(ret); self.generics.threads.len() - 1 })
    }

    fn specialization_type_arguments(
        &mut self,
        name_offset: usize,
        limit: usize,
        parse_argument: impl Fn(&mut Self, usize) -> Result<Type, ae_span::Diagnostic>,
    ) -> Result<Vec<Type>, ae_span::Diagnostic> {
        let shared = self.shared_specialization_arguments(name_offset);
        if let Some(ranges) = shared.as_deref()
            && ranges.len() <= limit
                && let Some(end) = self.resolved_specialization_end(ranges)
                    && let Some(arguments) = self.shared_specialization_types(ranges) {
                        self.pos = end;
                        return Ok(arguments);
                    }
        self.pos += 1;
        let mut arguments = Vec::new();
        if let Some(ranges) = shared {
            // Use the shared argument boundaries while the compiler resolves types.
            // If a type ends earlier in the typed grammar, finish at that cursor
            // with the same delimiter and diagnostics as the token path.
            for (index, range) in ranges.iter().enumerate() {
                if self.tokens[self.pos].offset != range.start {
                    break;
                }
                arguments.push(parse_argument(self, index)?);
                let last = index + 1 == ranges.len();
                let delimiter = if last { Kind::RBracket } else { Kind::Comma };
                if self.tokens[self.pos - 1].end == range.end && *self.current() == delimiter {
                    self.pos += 1;
                    if last {
                        return Ok(arguments);
                    }
                    continue;
                }
                if *self.current() != Kind::Comma {
                    self.take(Kind::RBracket)?;
                    return Ok(arguments);
                }
                self.pos += 1;
                break;
            }
        }
        loop {
            arguments.push(parse_argument(self, arguments.len())?);
            if *self.current() != Kind::Comma {
                break;
            }
            self.pos += 1;
        }
        self.take(Kind::RBracket)?;
        Ok(arguments)
    }

    /// Read the type arguments of an explicit generic call (`f[i64](..)`) for
    /// the resolved body route: same reader and diagnostics as the typed
    /// call site.
    pub(super) fn generic_call_type_arguments(&mut self, name_offset: usize) -> Result<Vec<Type>, ae_span::Diagnostic> {
        self.specialization_type_arguments(name_offset, Self::MAX_GENERIC_CALL_TYPE_ARGUMENTS, Self::generic_call_type_argument)
    }

    // Verify the exact token boundaries before trusting the indexed types.
    // The caller remains at the committed specialization site.
    fn resolved_specialization_end(&self, ranges: &[std::ops::Range<usize>]) -> Option<usize> {
        if ranges.is_empty() || *self.current() != Kind::LBracket {
            return None;
        }
        let mut cursor = self.pos + 1;
        for (index, range) in ranges.iter().enumerate() {
            if self.tokens.get(cursor)?.offset != range.start {
                return None;
            }
            let end = self.tokens.partition_point(|token| token.offset < range.end);
            let delimiter = if index + 1 == ranges.len() { Kind::RBracket } else { Kind::Comma };
            if end <= cursor || self.tokens[end - 1].end != range.end || self.tokens.get(end)?.kind != delimiter {
                return None;
            }
            cursor = end + 1;
        }
        Some(cursor)
    }

    const MAX_GENERIC_CALL_TYPE_ARGUMENTS: usize = 8;

    fn generic_call_type_argument(&mut self, count: usize) -> Result<Type, ae_span::Diagnostic> {
        if count >= Self::MAX_GENERIC_CALL_TYPE_ARGUMENTS {
            return Err(self.fail("type argument limit exceeded (8)"));
        }
        self.named_type()
    }

    pub(super) fn expression(&mut self, min: u8, moving: bool) -> Result<(Expr, Type), ae_span::Diagnostic> {
        if self.expression_depth == 0 {
            self.expression_nodes = 0;
        }
        if self.expression_depth >= 128 { return Err(self.fail("expression nesting limit exceeded (128)")); }
        if min == 0
            && let Some(value) = self.lower_shared_value(None, moving) {
                return Ok(value);
            }
        self.expression_depth += 1;
        let result = self.expression_inner(min, moving);
        self.expression_depth -= 1;
        result
    }
    pub(super) fn expression_inner(&mut self, min: u8, moving: bool) -> Result<(Expr, Type), ae_span::Diagnostic> {
        use ae_syntax::operator::{parse_infix_tail, prefix_binding_power};

        self.expression_nodes += 1;
        if self.expression_nodes > 128 { return Err(self.fail("expression complexity limit exceeded (128)")); }
        let mut lhs = match self.current().clone() {
            Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False | Kind::Text(_) => {
                use ae_syntax::literal::{parse_literal, LiteralValue};

                let literal = parse_literal(&self.tokens[self.pos]).expect("literal token");
                self.pos += 1;
                match literal.value {
                    LiteralValue::Integer(value) => (Expr::Number(value), Type::Int),
                    LiteralValue::Float(value) => (Expr::FloatLit(value), Type::F64),
                    LiteralValue::Boolean(value) => (Expr::BoolLit(value), Type::Bool),
                    LiteralValue::String(value) => {
                        (Expr::StrLit(value.to_owned(), literal.line, literal.column), Type::String)
                    }
                }
            }
            // A closure starts with a pipe — or with `||`, which the lexer
            // keeps as Or: an or-expression is never followed by an arrow, so
            // Or + '->' can only be an empty parameter list (ADR-0188).
            Kind::Pipe => self.closure_literal()?,
            Kind::Or if self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::Arrow) => self.closure_literal()?,
            operator if prefix_binding_power(&operator).is_some() => {
                let (line, column) = self.position();
                self.pos += 1;
                let power = prefix_binding_power(&operator).expect("prefix operator");
                let (operand, ty) = self.expression(power, false)?;
                self.lower_prefix(operator, (operand, ty), (line, column), None)?
            }
            // A borrow expression exists only to be cast (M2.12 slice 2):
            // `&expr as *const T` takes the address of addressable storage.
            // A general borrow expression is a later slice; the address of a
            // temporary is meaningless, so only variables and their fields
            // are accepted.
            Kind::Amp => {
                let (line, column) = self.position();
                self.pos += 1;
                let mutable_borrow = if *self.current() == Kind::Mut { self.pos += 1; true } else { false };
                let (operand, ty) = self.expression(6, false)?;
                if !matches!(operand, Expr::Variable(..)) {
                    return Err(self.fail("only the address of a local variable can be taken; bind the value to a variable first"));
                }
                if let Expr::Variable(id, ..) = &operand {
                    self.check_address_binding(*id, mutable_borrow)?;
                }
                if *self.current() != Kind::As {
                    return Err(self.fail("a borrow expression must be cast to a pointer; write '&expr as *const T' or '&mut expr as *mut T'"));
                }
                self.pos += 1;
                let (_, pointee) = self.pointer_cast_pointee(Some(mutable_borrow))?;
                self.lower_address_cast(operand, ty, pointee, mutable_borrow, (line, column))?
            }
            Kind::Ident(name) => {
                let name_offset = self.tokens[self.pos].offset;
                let (vline, vcol) = self.position();
                self.pos += 1;
                if name == "vec" && *self.current() == Kind::LBracket {
                    let (line, column) = (vline, vcol);
                    self.pos += 1;
                    let mut element_ty: Option<Type> = None;
                    let values = if *self.current() == Kind::RBracket {
                        Vec::new()
                    } else {
                        ae_syntax::parse_comma_list(
                            self,
                            |parser, count| {
                                if count >= 65536 {
                                    return Err(parser.fail("vector element limit exceeded (65536)"));
                                }
                                let (element, ty) = parser.expression(0, false)?;
                                // The first element decides the vector: all i64 is
                                // `vec`, all f64 is `vec[f64]` (ADR-61). Mixing
                                // them is an error — the language has no implicit
                                // conversion, including inside a literal.
                                element_ty = Some(parser.check_numeric_vector_element(element_ty, ty)?);
                                Ok(element)
                            },
                            |parser| {
                                if *parser.current() != Kind::Comma {
                                    return false;
                                }
                                parser.pos += 1;
                                true
                            },
                        )?
                    };
                    // Empty literal defaults to the i64 vector; the f64
                    // elements chose vec[f64] (ADR-61).
                    let vector_ty = match element_ty {
                        Some(Type::F64) => Type::VecF,
                        _ => Type::Vec,
                    };
                    self.take(Kind::RBracket)?;
                    // Bind and fall through to the binary-operator loop.
                    (Expr::VecLit(values, vector_ty, line, column), vector_ty)
                } else if *self.current() == Kind::ColonColon {
                    // `Enum::Variant` / `Enum::Variant(payload, ...)`, or
                    // `Type::assoc_fn(...)` from the same impl block
                    // (ADR-0183, ADR-0186). The name after `::` decides: a
                    // variant name is a variant path, anything else reaches
                    // the impl — shallow, deterministic, no guessing.
                    if name == "Self" {
                        let Some(target) = self.impl_self else {
                            return Err(self.error_at(name_offset, "'Self' is only available inside an impl block"));
                        };
                        let type_name = match target {
                            Type::Struct(sid) => self.structs[sid].name.clone(),
                            Type::Enum(ename, _) => ename.to_owned(),
                            other => return Err(self.error_at(name_offset, &format!("'Self' is {} here and has no associated functions", other.name(self.ctx)))),
                        };
                        // For an enum Self, a variant name after `::` is a
                        // variant path, exactly like a bare enum name; an
                        // associated function resolves through the template.
                        if let Type::Enum(ename, eid) = target {
                            let def = &self.enums[eid];
                            let next_is_variant = match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
                                Some(Kind::Ident(v)) => def.variants.iter().any(|(vn, _)| vn == v),
                                _ => false,
                            };
                            if next_is_variant {
                                self.enum_literal(eid, name_offset)?
                            } else if let Some((gid, origin_args)) = self.generics.enum_origin.get(&eid).cloned() {
                                // `Self::assoc(...)` on a generic enum:
                                // resolve through the declaration's template
                                // with the instantiation's own arguments.
                                self.take(Kind::ColonColon)?;
                                let method = self.identifier()?;
                                let template = self.generics.defs[gid].name.clone();
                                let origin: Vec<Type> = origin_args;
                                let Some(gimpl) = self.generics.generic_impls.get(&template).cloned() else {
                                    return Err(self.error_at(name_offset, &format!("'{}' has no generic impl block", template)));
                                };
                                let Some(mdef) = gimpl.methods.iter().find(|m| m.name == method).cloned() else {
                                    let mut names: Vec<String> = gimpl.methods.iter().map(|m| m.name.clone()).collect();
                                    names.sort();
                                    return Err(self.error_at(name_offset, &format!("the impl for '{template}' has no associated function '{method}'; it has {}", names.join(", "))));
                                };
                                let has_receiver = mdef.receiver.is_some();
                                let instance = self.instantiate_impl_method(&template, &method, eid, gid, &origin, name_offset)?;
                                let (id, params, ret) = instance;
                                let call_params = if has_receiver { params[1..].to_vec() } else { params.clone() };
                                let call_args = self.call_arguments(&format!("{ename}::{method}"), &call_params)?;
                                (Expr::Call(id, call_args, ret), ret)
                            } else {
                                // A plain enum's impl block: the same lookup a
                                // bare `Shape::circle(...)` outside the impl
                                // uses (ADR-0186).
                                self.associated_call(&type_name, name_offset)?
                            }
                        } else {
                            self.associated_call(&type_name, name_offset)?
                        }
                    } else if let Some(&eid) = self.enum_names.get(&name) {
                        let def = &self.enums[eid];
                        let next_is_variant = match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
                            Some(Kind::Ident(v)) => def.variants.iter().any(|(vn, _)| vn == v),
                            _ => false,
                        };
                        if next_is_variant {
                            self.enum_literal(eid, name_offset)?
                        } else {
                            self.associated_call(&name, name_offset)?
                        }
                    } else if self.struct_names.contains_key(&name) {
                        self.associated_call(&name, name_offset)?
                    } else {
                        return Err(self.error_at(name_offset, &format!("'{name}' is not an enum or a type with an impl block; '::' reaches enum variants and associated functions, nothing else")));
                    }
                } else if self.generics.names.contains_key(&name) && *self.current() == Kind::LBracket {
                    // Generic enum literal: the arguments pick the layout, so
                    // Option[string]::Some and Option[i64]::Some are different
                    // variants of different types (ADR-21).
                    let gid = self.generics.names[&name];
                    let args = self.specialization_type_arguments(name_offset, usize::MAX, |parser, _| parser.type_argument())?;
                    let eid = self.instantiate_enum(gid, args.clone(), name_offset)?;
                    // `::` after the arguments: a variant name stays a variant
                    // path (enum_literal consumes `::` itself); anything else
                    // resolves through the template's associated functions
                    // (ADR-0196).
                    let next_ident = match self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) {
                        Some(Kind::Ident(v)) => Some(v),
                        _ => None,
                    };
                    let next_is_variant = next_ident.as_deref()
                        .map(|v| self.generics.defs[gid].variants.iter().any(|(vn, _)| vn == v))
                        .unwrap_or(false);
                    if *self.current() == Kind::ColonColon && !next_is_variant {
                        self.pos += 1; // consume `::`
                            self.pos += 1; // skip the method name
                        let method = next_ident.unwrap_or_default();
                        let display = self.enums[eid].name;
                        let template = self.generics.defs[gid].name.clone();
                        let origin: Vec<Type> = args.clone();
                        let Some(gimpl) = self.generics.generic_impls.get(&template).cloned() else {
                            return Err(self.error_at(name_offset, &format!("'{}' has no generic impl block, so it has no associated functions", template)));
                        };
                        let Some(mdef) = gimpl.methods.iter().find(|m| m.name == method).cloned() else {
                            let mut names: Vec<String> = gimpl.methods.iter().map(|m| m.name.clone()).collect();
                            names.sort();
                            return Err(self.error_at(name_offset, &format!("the impl for '{template}' has no associated function '{method}'; it has {}", names.join(", "))));
                        };
                        let has_receiver = mdef.receiver.is_some();
                        let instance = self.instantiate_impl_method(&template, &method, eid, gid, &origin, name_offset)?;
                        let (id, params, ret) = instance;
                        // `call_arguments` starts at '('; assert the shape.
                        if *self.current() != Kind::LParen {
                            return Err(self.error_at(name_offset, &format!("'{method}' expects an argument list")));
                        }
                        let call_params = if has_receiver { params[1..].to_vec() } else { params.clone() };
                        let call_args = self.call_arguments(&format!("{display}::{method}"), &call_params)?;
                        (Expr::Call(id, call_args, ret), ret)
                    } else {
                        self.enum_literal(eid, name_offset)?
                    }
                } else if self.generics.struct_names.contains_key(&name) && *self.current() == Kind::LBracket {
                    // Generic struct literal: the arguments pick the layout, so
                    // Pair[string, i64] and Pair[i64, i64] are different types.
                    let gid = self.generics.struct_names[&name];
                    let args = self.specialization_type_arguments(name_offset, usize::MAX, |parser, _| parser.named_type())?;
                    let sid = self.instantiate_struct(gid, args, name_offset)?;
                    self.require_public("struct", &self.structs[sid].name.clone(), &self.structs[sid].file.clone(), self.structs[sid].public)?;
                    if *self.current() == Kind::ColonColon {
                        // `Pair[string, i64]::new(...)` (ADR-0187): the type
                        // arguments pick both the layout and the method body.
                        self.pos += 1;
                        let method = self.identifier()?;
                        let display = self.structs[sid].name.clone();
                        let Some(&(origin_gid, ref origin_args)) = self.generics.struct_origin.get(&sid) else {
                            return Err(self.error_at(name_offset, "internal: a generic instantiation has no origin"));
                        };
                        let origin: Vec<Type> = origin_args.clone();
                        let def_name = self.generics.struct_defs[origin_gid].name.clone();
                        let Some(gimpl) = self.generics.generic_impls.get(&def_name).cloned() else {
                            return Err(self.error_at(name_offset, &format!("'{}' has no generic impl block, so it has no associated functions", def_name)));
                        };
                        let Some(mdef) = gimpl.methods.iter().find(|m| m.name == method).cloned() else {
                            let mut names: Vec<String> = gimpl.methods.iter().map(|m| m.name.clone()).collect();
                            names.sort();
                            return Err(self.error_at(name_offset, &format!("the impl for '{def_name}' has no associated function '{method}'; it has {}", names.join(", "))));
                        };
                        let has_receiver = mdef.receiver.is_some();
                        let instance = self.instantiate_impl_method(&def_name, &method, sid, origin_gid, &origin, name_offset)?;
                        let (id, params, ret) = instance;
                        let call_params = if has_receiver { params[1..].to_vec() } else { params.clone() };
                        let call_args = self.call_arguments(&format!("{display}::{method}"), &call_params)?;
                        // A tail value, not an early return: the postfix loop
                        // still owns chaining (Pair[string, i64]::new(..).m()).
                        (Expr::Call(id, call_args, ret), ret)
                    } else if *self.current() == Kind::LBrace {
                        match self.lower_committed_specialized_struct(sid) {
                            Some(value) => value,
                            None => self.struct_literal(sid, name_offset)?,
                        }
                    } else {
                        return Err(self.error_at(name_offset, &format!("'{}' is a struct; construct it with '{}{{ ... }}'", self.structs[sid].name.clone(), self.structs[sid].name.clone())));
                    }
                } else if name == "Self" && (*self.current() == Kind::LBrace || *self.current() == Kind::ColonColon) {
                    // `Self` in an expression (ADR-0186): the impl's type as a
                    // constructor — `Self { ... }` or `Self::assoc(...)`.
                    let Some(target) = self.impl_self else {
                        return Err(self.error_at(name_offset, "'Self' is only available inside an impl block"));
                    };
                    match target {
                        Type::Struct(sid) => {
                            if *self.current() == Kind::LBrace {
                                return self.struct_literal(sid, name_offset);
                            }
                            let sname = self.structs[sid].name.clone();
                            return self.associated_call(&sname, name_offset);
                        }
                        Type::Enum(ename, _) => {
                            return self.associated_call(ename, name_offset);
                        }
                        other => return Err(self.error_at(name_offset, &format!("'Self' is {} here and has no constructor", other.name(self.ctx)))),
                    }
                } else if let Some(&sid) = self.struct_names.get(&name) {
                    self.require_public("struct", &self.structs[sid].name, &self.structs[sid].file, self.structs[sid].public)?;
                    if *self.current() == Kind::LBrace {
                        // A fresh literal takes no postfix; wrap it in
                        // parentheses for field access.
                        return self.struct_literal(sid, name_offset);
                    } else if *self.current() == Kind::LParen {
                        return Err(self.fail(&format!("'{}' is a struct, not a function", name)));
                    } else {
                        return Err(self.fail(&format!("'{}' is a struct name; construct it with '{} {{ ... }}'", name, name)));
                    }
                } else if name == "len" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    // Fast paths for bare variables: array length is a
                    // compile-time constant; vec length borrows the slot.
                    let mut fast: Option<(Expr, Type)> = None;
                    if let Kind::Ident(arg_name) = self.current().clone() {
                        let after = self.tokens.get(self.pos + 1).map(|t| t.kind.clone());
                        if after == Some(Kind::RParen)
                            && let Some(&(id, _, ty)) = self.names.get(&arg_name) {
                                if self.moved.contains(&id) { return Err(moved_value_hint(self.binding_label(self.error_at(name_offset, &format!("cannot use '{arg_name}' because it was moved")).with_code("E0102"), id))); }
                                match ty {
                                    Type::Array(n) => { self.pos += 2; fast = Some((Expr::Number(n as i64), Type::Int)); }
                                    Type::Vec | Type::StrVec | Type::VecRef | Type::StrVecRef | Type::Map => {
                                        self.pos += 2;
                                        fast = Some((Expr::Len(Box::new(Expr::Variable(id, ty, vline, vcol)), line, col), Type::Int));
                                    }
                                    _ => {}
                                }
                            }
                    }
                    match fast {
                        Some(pair) => pair,
                        None => {
                            let (operand, ty) = self.expression(0, false)?;
                            self.take(Kind::RParen)?;
                            // A token cursor has no length by design: counting
                            // would consume the stream, and the whole point of
                            // the cursor is not materialising the tokens.
                            if ty == Type::Tokens { return Err(self.fail("a token cursor has no len: counting would consume the stream, so use split(text, sep) when the count is needed")); }
                            if !matches!(ty, Type::String | Type::StrView | Type::Vec | Type::StrVec | Type::VecF | Type::VecRef | Type::VecFRef | Type::Map | Type::MapF) { return Err(self.fail("len expects a string, vec, map, or array variable")); }
                            (Expr::Len(Box::new(operand), line, col), Type::Int)
                        }
                    }
                } else if name == "ptr_write" && *self.current() == Kind::LParen {
                    if self.unsafe_depth == 0 {
                        return Err(self.error_at(name_offset, "ptr_write is an unsafe operation; wrap it in an 'unsafe { ... }' block"));
                    }
                    self.pos += 1;
                    let (ptr_expr, ptr_ty) = self.expression(0, false)?;
                    self.take(Kind::Comma)?;
                    let (val_expr, val_ty) = self.expression(0, true)?;
                    self.take(Kind::RParen)?;
                    let Type::PtrMut(pid) = ptr_ty else {
                        return Err(self.error_at(name_offset, "ptr_write expects a '*mut T' pointer as its first argument"));
                    };
                    let pointee = self.ctx.ptr_pointee(pid);
                    if val_ty != pointee {
                        return Err(self.error_at(name_offset, &format!("ptr_write value type mismatch: expected '{}', got '{}'", pointee.name(self.ctx), val_ty.name(self.ctx))));
                    }
                    let (line, column) = self.position();
                    (Expr::PtrWrite(Box::new(ptr_expr), Box::new(val_expr), pointee, line, column), Type::Int)
                } else if name == "ptr_drop_in_place" && *self.current() == Kind::LParen {
                    if self.unsafe_depth == 0 {
                        return Err(self.error_at(name_offset, "ptr_drop_in_place is an unsafe operation; wrap it in an 'unsafe { ... }' block"));
                    }
                    self.pos += 1;
                    let (ptr_expr, ptr_ty) = self.expression(0, false)?;
                    self.take(Kind::RParen)?;
                    let Type::PtrMut(pid) = ptr_ty else {
                        return Err(self.error_at(name_offset, "ptr_drop_in_place expects a '*mut T' pointer"));
                    };
                    let pointee = self.ctx.ptr_pointee(pid);
                    let (line, column) = self.position();
                    (Expr::PtrDropInPlace(Box::new(ptr_expr), pointee, line, column), Type::Int)
                } else if (name == "c_size_of" || name == "c_align_of" || name == "c_offset_of") && *self.current() == Kind::LBracket {
                    self.c_layout_expr(&name)?
                } else if name == "slice_view" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (base, base_ty) = self.expression(0, false)?;
                    // The view borrows its base. A bare string variable is the
                    // direct case; a string field of a *variable* struct is
                    // allowed too, and borrows the whole variable: any move,
                    // field write or replacement of that variable would free
                    // the bytes the view points at (ADR-52).
                    let (vid, field) = match (&base, base_ty) {
                        (Expr::Variable(id, Type::String, _, _), Type::String) => (*id, None),
                        (Expr::Field(inner, fname, Type::String), Type::String) => match **inner {
                            Expr::Variable(id, Type::Struct(_), _, _) => (id, Some(fname.clone())),
                            _ => return Err(self.fail("slice_view can follow a string field only on a struct variable, not on a nested expression")),
                        },
                        _ => return Err(self.fail("slice_view expects a string variable or a string field of a struct variable (views borrow their base)")),
                    };
                    if self.moved.contains(&vid) { return Err(self.binding_label(self.fail("cannot use a moved string in slice_view").with_code("E0102"), vid)); }
                    self.take(Kind::Comma)?;
                    let (start, start_ty) = self.expression(0, false)?;
                    if start_ty != Type::Int { return Err(self.fail("slice_view start must be i64")); }
                    self.take(Kind::Comma)?;
                    let (end, end_ty) = self.expression(0, false)?;
                    if end_ty != Type::Int { return Err(self.fail("slice_view end must be i64")); }
                    self.take(Kind::RParen)?;
                    // The base may not move or be reassigned while a view of
                    // it can still be read (until this scope ends).
                    let depth = self.scopes.len();
                    if !self.borrows.iter().any(|&(id, _, _)| id == vid) {
                        self.borrows.push((vid, depth, BorrowKind::View));
                    }
                    (Expr::SliceView(vid, field, Box::new(start), Box::new(end), line, col), Type::StrView)
                } else if name == "tokenize" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (base, base_ty) = self.expression(0, false)?;
                    // The cursor reads through a view of its base, so the base
                    // must be storage that outlives the loop: a string variable
                    // or a string field of a struct variable. A temporary (a
                    // concatenation, a call result) would be freed before the
                    // first iteration, so it is rejected (ADR-53).
                    let (vid, field) = match (&base, base_ty) {
                        (Expr::Variable(id, Type::String, _, _), Type::String) => (*id, None),
                        (Expr::Field(inner, fname, Type::String), Type::String) => match **inner {
                            Expr::Variable(id, Type::Struct(_), _, _) => (id, Some(fname.clone())),
                            _ => return Err(self.fail("tokenize can follow a string field only on a struct variable")),
                        },
                        _ => return Err(self.fail("tokenize expects a string variable or a string field of a struct variable (the cursor borrows its base)")),
                    };
                    if self.moved.contains(&vid) { return Err(self.binding_label(self.fail("cannot tokenize a moved string").with_code("E0102"), vid)); }
                    self.take(Kind::Comma)?;
                    let (sep, sep_ty) = self.expression(0, false)?;
                    if !matches!(sep_ty, Type::String | Type::StrView) { return Err(self.fail("tokenize expects a string or string view separator")); }
                    self.take(Kind::RParen)?;
                    // Same rule as a view: the base may not move or be
                    // reassigned while the cursor can still read it.
                    let depth = self.scopes.len();
                    if !self.borrows.iter().any(|&(id, _, _)| id == vid) {
                        self.borrows.push((vid, depth, BorrowKind::View));
                    }
                    (Expr::Tokens(vid, field, Box::new(sep), line, col), Type::Tokens)
                } else if name == "count_lines_with" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (text, text_ty) = self.expression(0, false)?;
                    if !matches!(text_ty, Type::String | Type::StrView) { return Err(self.fail("count_lines_with expects a string or string view")); }
                    self.take(Kind::Comma)?;
                    let (pat, pat_ty) = self.expression(0, false)?;
                    if !matches!(pat_ty, Type::String | Type::StrView) { return Err(self.fail("count_lines_with expects a string or string view pattern")); }
                    self.take(Kind::RParen)?;
                    (Expr::CountLinesWith(Box::new(text), Box::new(pat), line, col), Type::Int)
                } else if name == "lines" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (text, text_ty) = self.expression(0, false)?;
                    if !matches!(text_ty, Type::String | Type::StrView) { return Err(self.fail("lines expects a string or string view")); }
                    self.take(Kind::RParen)?;
                    (Expr::Lines(Box::new(text), line, col), Type::StrVec)
                } else if name == "join" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    // join borrows a bare string-vector variable.
                    let (parts, parts_ty) = self.expression(0, false)?;
                    let vid = match (&parts, parts_ty) {
                        (Expr::Variable(id, Type::StrVec, _, _), Type::StrVec) => *id,
                        _ => return Err(self.fail("join expects a bare vec[string] variable")),
                    };
                    if self.moved.contains(&vid) { return Err(self.binding_label(self.fail("cannot use a moved vector in join").with_code("E0102"), vid)); }
                    self.take(Kind::Comma)?;
                    let (sep, sep_ty) = self.expression(0, false)?;
                    if !matches!(sep_ty, Type::String | Type::StrView) { return Err(self.fail("join expects a string or string view separator")); }
                    self.take(Kind::RParen)?;
                    (Expr::Join(vid, Box::new(sep), line, col), Type::String)
                } else if name == "cell_new" && *self.current() == Kind::LParen {
                    // Value arms: the result must flow into the infix tail so a
                    // call can participate in a larger expression
                    // (`cell_load(c) == 1` — the early `return` here used to
                    // end the expression at the call, ADR-0224 R2.4 found it).
                    self.cell_new_expr(vline, vcol)?
                } else if name == "cell_load" && *self.current() == Kind::LParen {
                    self.cell_load_expr(vline, vcol)?
                } else if name == "cell_add" && *self.current() == Kind::LParen {
                    self.cell_add_expr(vline, vcol)?
                } else if name == "cell_cas" && *self.current() == Kind::LParen {
                    self.cell_cas_expr(vline, vcol)?
                } else if matches!(name.as_str(), "shared_new" | "shared_get" | "mutex_new" | "mutex_lock" | "chan_new" | "chan_send" | "chan_try_send" | "chan_recv" | "chan_select" | "exec_new" | "exec_run" | "scope_new" | "pool_new" | "pool_join") && *self.current() == Kind::LParen {
                    self.concurrency_builtin_expr(&name, vline, vcol)?
                } else if name == "spawn" && *self.current() == Kind::LParen {
                    self.spawn_expr(vline, vcol)?
                } else if name == "wait" && *self.current() == Kind::LParen {
                    self.wait_expr(vline, vcol)?
                } else if name == "args" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (Expr::Args(line, col), Type::StrVec)
                } else if name == "try_read_file" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("ReadResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("try_read_file expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::TryReadFile(Box::new(path), ty, line, col), ty)
                } else if name == "try_write_file" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("WriteResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("try_write_file expects a string path")); }
                    self.take(Kind::Comma)?;
                    let (content, content_ty) = self.expression(0, false)?;
                    if !matches!(content_ty, Type::String | Type::StrView) { return Err(self.fail("try_write_file expects a string or string view content")); }
                    self.take(Kind::RParen)?;
                    (Expr::TryWriteFile(Box::new(path), Box::new(content), ty, line, col), ty)
                } else if name == "try_append_file" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("WriteResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("try_append_file expects a string path")); }
                    self.take(Kind::Comma)?;
                    let (content, content_ty) = self.expression(0, false)?;
                    if !matches!(content_ty, Type::String | Type::StrView) { return Err(self.fail("try_append_file expects a string or string view content")); }
                    self.take(Kind::RParen)?;
                    (Expr::TryAppendFile(Box::new(path), Box::new(content), ty, line, col), ty)
                } else if name == "env" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("ReadResult");
                    self.take(Kind::LParen)?;
                    let (variable, variable_ty) = self.expression(0, false)?;
                    if !matches!(variable_ty, Type::String | Type::StrView) { return Err(self.fail("env expects a string variable name")); }
                    self.take(Kind::RParen)?;
                    (Expr::Env(Box::new(variable), ty, line, col), ty)
                } else if (name == "read_stdin" || name == "cwd") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("ReadResult");
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (if name == "read_stdin" { Expr::ReadStdin(ty, line, col) } else { Expr::Cwd(ty, line, col) }, ty)
                } else if name == "f64vec_new" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (Expr::F64VecNew(line, col), Type::VecF)
                } else if name == "strvec_new" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (Expr::StrVecNew(line, col), Type::StrVec)
                } else if name == "run_command" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("CommandResult");
                    self.take(Kind::LParen)?;
                    let (program, program_ty) = self.expression(0, false)?;
                    if !matches!(program_ty, Type::String | Type::StrView) { return Err(self.fail("run_command expects a string program name")); }
                    self.take(Kind::Comma)?;
                    // Like join, the argument vector is borrowed, never moved:
                    // the callee only reads the elements during the call.
                    let (args, args_ty) = self.expression(0, false)?;
                    let vid = match (&args, args_ty) {
                        (Expr::Variable(id, Type::StrVec, _, _), Type::StrVec) => *id,
                        _ => return Err(self.fail("run_command expects a bare vec[string] variable for its arguments")),
                    };
                    if self.moved.contains(&vid) { return Err(self.binding_label(self.fail("cannot use a moved vector in run_command").with_code("E0102"), vid)); }
                    self.take(Kind::RParen)?;
                    (Expr::RunCommand(Box::new(program), vid, ty, line, col), ty)
                } else if name == "find" && *self.current() == Kind::LParen {
                    // Byte offset of the first match, as a result value: a -1
                    // sentinel would let callers ignore "not found", while
                    // every other fallible built-in returns a result value
                    // whose error must be handled.
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("IntResult");
                    self.take(Kind::LParen)?;
                    let (haystack, haystack_ty) = self.expression(0, false)?;
                    if !matches!(haystack_ty, Type::String | Type::StrView) { return Err(self.fail("find expects a string to search in")); }
                    self.take(Kind::Comma)?;
                    let (needle, needle_ty) = self.expression(0, false)?;
                    if !matches!(needle_ty, Type::String | Type::StrView) { return Err(self.fail("find expects a string to search for")); }
                    self.take(Kind::RParen)?;
                    (Expr::Find(Box::new(haystack), Box::new(needle), ty, line, col), ty)
                } else if (name == "json_get" || name == "json_int" || name == "json_bool" || name == "json_len" || name == "json_has") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = match name.as_str() {
                        "json_get" => self.prelude_type("ReadResult"),
                        "json_has" => Type::Bool,
                        "json_bool" => self.prelude_type("BoolResult"),
                        _ => self.prelude_type("IntResult"),
                    };
                    self.take(Kind::LParen)?;
                    let (doc, doc_ty) = self.expression(0, false)?;
                    if !matches!(doc_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a JSON document string"))); }
                    self.take(Kind::Comma)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string path"))); }
                    self.take(Kind::RParen)?;
                    let (doc, path) = (Box::new(doc), Box::new(path));
                    (match name.as_str() {
                        "json_get" => Expr::JsonGet(doc, path, ty, line, col),
                        "json_int" => Expr::JsonInt(doc, path, ty, line, col),
                        "json_bool" => Expr::JsonBool(doc, path, ty, line, col),
                        "json_len" => Expr::JsonLen(doc, path, ty, line, col),
                        _ => Expr::JsonHas(doc, path, line, col),
                    }, ty)
                } else if (name == "now_ms" || name == "now_s") && *self.current() == Kind::LParen {
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (if name == "now_ms" { Expr::NowMs } else { Expr::NowS }, Type::Int)
                } else if name == "try_list_dir" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("DirResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("try_list_dir expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::TryListDir(Box::new(path), ty, line, col), ty)
                } else if name == "path_is_dir" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("path_is_dir expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::PathIsDir(Box::new(path), line, col), Type::Bool)
                } else if name == "path_exists" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("path_exists expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::PathExists(Box::new(path), line, col), Type::Bool)
                } else if name == "map_new" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (Expr::MapNew(line, col, MapKind::Int), Type::Map)
                } else if name == "f64map_new" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    self.take(Kind::RParen)?;
                    (Expr::MapNew(line, col, MapKind::Float), Type::MapF)
                } else if matches!(name.as_str(), "map_get" | "map_has" | "map_remove" | "map_inc" | "keys") && *self.current() == Kind::LParen {
                    if name == "map_inc" {
                        // `map_inc(m, key, delta)` is the counting compound:
                        // read-modify-write with missing keys starting from
                        // zero, returning the new value. One lookup instead
                        // of a get plus a put.
                        let (line, col) = (vline, vcol);
                        self.take(Kind::LParen)?;
                        let (map, map_ty) = self.expression(0, false)?;
                        let kind = map_kind_of(map_ty);
                        let place = match (&map, map_ty) {
                            (Expr::Variable(id, ty2, _, _), t) if *ty2 == t && matches!(t, Type::Map | Type::MapF) => MapPlace::Var(*id, kind),
                            (Expr::Field(base, fname, t), _) if matches!(*t, Type::Map | Type::MapF) => match **base {
                                Expr::Variable(id, _, _, _) => MapPlace::Field(id, fname.clone(), kind),
                                _ => return Err(self.fail("map_inc needs a map variable or a field of one")),
                            },
                            _ => return Err(self.fail("map_inc expects a map variable or a field of one")),
                        };
                        if self.moved.contains(&place.slot()) { return Err(self.binding_label(self.fail("cannot use 'map_inc' on a moved map").with_code("E0102"), place.slot())); }
                        self.take(Kind::Comma)?;
                        let (key, key_ty) = self.expression(0, false)?;
                        if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail("map_inc expects a string key")); }
                        self.take(Kind::Comma)?;
                        let (delta, delta_ty) = self.expression(0, false)?;
                        if delta_ty != map_element_ty(kind) { return Err(self.fail(&format!("map_inc expects a {} delta", map_element_ty(kind).name(self.ctx)))); }
                        self.take(Kind::RParen)?;
                        (Expr::MapInc(place, Box::new(key), Box::new(delta), line, col), map_element_ty(kind))
                    } else {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    // A bare map variable keeps the borrow explicit: the map is
                    // read (or mutated) only for the duration of this call.
                    let (map, map_ty) = self.expression(0, false)?;
                    let kind = map_kind_of(map_ty);
                    let ty = match name.as_str() {
                        "map_get" => self.prelude_type(if kind == MapKind::Float { "FloatResult" } else { "IntResult" }),
                        "keys" => Type::StrVec,
                        _ => Type::Bool,
                    };
                    let place = match (&map, map_ty) {
                        (Expr::Variable(id, ty2, _, _), t) if *ty2 == t && matches!(t, Type::Map | Type::MapF) => MapPlace::Var(*id, kind),
                        (Expr::Field(base, fname, t), _) if matches!(*t, Type::Map | Type::MapF) => match **base {
                            Expr::Variable(id, _, _, _) => MapPlace::Field(id, fname.clone(), kind),
                            _ => return Err(self.fail(&format!("{name} needs a map variable or a field of one"))),
                        },
                        _ => return Err(self.fail(&format!("{name} expects a map variable or a field of one"))),
                    };
                    if self.moved.contains(&place.slot()) { return Err(self.binding_label(self.fail(&format!("cannot use '{name}' on a moved map")).with_code("E0102"), place.slot())); }
                    if name == "keys" {
                        self.take(Kind::RParen)?;
                        (Expr::MapKeys(place, line, col), ty)
                    } else {
                        self.take(Kind::Comma)?;
                        let (key, key_ty) = self.expression(0, false)?;
                        if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string key"))); }
                        self.take(Kind::RParen)?;
                        let builtin = match name.as_str() {
                            "map_get" => Expr::MapGet(place, Box::new(key), ty, line, col),
                            "map_has" => Expr::MapHas(place, Box::new(key), line, col),
                            _ => Expr::MapRemove(place, Box::new(key), line, col),
                        };
                        (builtin, ty)
                    }
                    }
                } else if matches!(name.as_str(), "utf8_len" | "utf8_prefix") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (text, text_ty) = self.expression(0, false)?;
                    if !matches!(text_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string or string view"))); }
                    if name == "utf8_len" {
                        self.take(Kind::RParen)?;
                        (Expr::Utf8Len(Box::new(text), line, col), Type::Int)
                    } else {
                        self.take(Kind::Comma)?;
                        let (count, count_ty) = self.expression(0, false)?;
                        if count_ty != Type::Int { return Err(self.fail("utf8_prefix expects an i64 character count")); }
                        self.take(Kind::RParen)?;
                        (Expr::Utf8Prefix(Box::new(text), Box::new(count), line, col), Type::String)
                    }
                } else if name == "path_join" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (dir, dir_ty) = self.expression(0, false)?;
                    if !matches!(dir_ty, Type::String | Type::StrView) { return Err(self.fail("path_join expects a string directory")); }
                    self.take(Kind::Comma)?;
                    let (name, name_ty) = self.expression(0, false)?;
                    if !matches!(name_ty, Type::String | Type::StrView) { return Err(self.fail("path_join expects a string name")); }
                    self.take(Kind::RParen)?;
                    (Expr::PathJoin(Box::new(dir), Box::new(name), line, col), Type::String)
                } else if name == "path_name" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("path_name expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::PathName(Box::new(path), line, col), Type::String)
                } else if name == "path_parent" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("path_parent expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::PathParent(Box::new(path), line, col), Type::String)
                } else if name == "try_file_info" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("FileResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail("try_file_info expects a string path")); }
                    self.take(Kind::RParen)?;
                    (Expr::TryFileInfo(Box::new(path), ty, line, col), ty)
                } else if matches!(name.as_str(), "try_copy_file" | "try_rename_path") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("StatusResult");
                    self.take(Kind::LParen)?;
                    let (from, from_ty) = self.expression(0, false)?;
                    if !matches!(from_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string source path"))); }
                    self.take(Kind::Comma)?;
                    let (to, to_ty) = self.expression(0, false)?;
                    if !matches!(to_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string destination path"))); }
                    self.take(Kind::RParen)?;
                    let builtin = if name == "try_copy_file" {
                        Expr::TryCopyFile(Box::new(from), Box::new(to), ty, line, col)
                    } else {
                        Expr::TryRenamePath(Box::new(from), Box::new(to), ty, line, col)
                    };
                    (builtin, ty)
                } else if matches!(name.as_str(), "try_remove_file" | "try_create_dir" | "try_remove_dir" | "set_cwd") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("StatusResult");
                    self.take(Kind::LParen)?;
                    let (path, path_ty) = self.expression(0, false)?;
                    if !matches!(path_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string path"))); }
                    self.take(Kind::RParen)?;
                    let builtin = if name == "try_remove_file" {
                        Expr::TryRemoveFile(Box::new(path), ty, line, col)
                    } else if name == "try_remove_dir" {
                        Expr::TryRemoveDir(Box::new(path), ty, line, col)
                    } else if name == "set_cwd" {
                        Expr::SetCwd(Box::new(path), ty, line, col)
                    } else {
                        Expr::TryCreateDir(Box::new(path), ty, line, col)
                    };
                    (builtin, ty)
                } else if name == "parse_int" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("IntResult");
                    self.take(Kind::LParen)?;
                    let (text, text_ty) = self.expression(0, false)?;
                    if !matches!(text_ty, Type::String | Type::StrView) { return Err(self.fail("parse_int expects a string or string view")); }
                    self.take(Kind::RParen)?;
                    (Expr::ParseInt(Box::new(text), ty, line, col), ty)
                } else if name == "to_string" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (value, value_ty) = self.expression(0, false)?;
                    let builtin = match value_ty {
                        Type::Int => Expr::ToString(Box::new(value), line, col),
                        Type::F64 => Expr::FloatToString(Box::new(value), line, col),
                        _ => return Err(self.fail("to_string expects an i64 or f64")),
                    };
                    self.take(Kind::RParen)?;
                    (builtin, Type::String)
                } else if name == "to_f64" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let (value, value_ty) = self.expression(0, false)?;
                    if value_ty != Type::Int { return Err(self.fail("to_f64 expects an i64")); }
                    self.take(Kind::RParen)?;
                    (Expr::ToF64(Box::new(value), line, col), Type::F64)
                } else if name == "to_i64" && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    let ty = self.prelude_type("IntResult");
                    self.take(Kind::LParen)?;
                    let (value, value_ty) = self.expression(0, false)?;
                    if value_ty != Type::F64 { return Err(self.fail("to_i64 expects an f64")); }
                    self.take(Kind::RParen)?;
                    (Expr::ToI64(Box::new(value), ty, line, col), ty)
                } else if MATH_FNS.iter().any(|(n, _, _)| *n == name.as_str()) && *self.current() == Kind::LParen {
                    // Math builtins (ADR-60): fixed arity, every argument must
                    // already be f64 — the language has no implicit conversion.
                    let index = MATH_FNS.iter().position(|(n, _, _)| *n == name.as_str()).unwrap();
                    let arity = MATH_FNS[index].2;
                    let (line, col) = (vline, vcol);
                    self.take(Kind::LParen)?;
                    let mut args: Vec<Box<Expr>> = Vec::new();
                    for position in 0..arity {
                        if *self.current() == Kind::RParen { return Err(self.fail(&format!("{name} expects {} f64 argument(s)", arity))); }
                        if position > 0 { self.take(Kind::Comma)?; }
                        let (arg, arg_ty) = self.expression(0, false)?;
                        if arg_ty != Type::F64 { return Err(self.fail(&format!("{name} expects an f64 argument; convert integers explicitly with to_f64"))); }
                        args.push(Box::new(arg));
                    }
                    if *self.current() == Kind::Comma { return Err(self.fail(&format!("{name} takes {} argument(s)", arity))); }
                    self.take(Kind::RParen)?;
                    (Expr::MathCall(index as u8, args, line, col), Type::F64)
                } else if matches!(name.as_str(), "read_file" | "write_file" | "bytes" | "contains" | "starts_with" | "ends_with" | "slice" | "split" | "trim") && *self.current() == Kind::LParen {
                    let (line, col) = (vline, vcol);
                    self.pos += 1;
                    let (first, first_ty) = self.expression(0, false)?;
                    if !matches!(first_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string or string view argument"))); }
                    let builtin = if name == "read_file" {
                        self.take(Kind::RParen)?;
                        Expr::ReadFile(Box::new(first), line, col)
                    } else if name == "bytes" {
                        self.take(Kind::RParen)?;
                        Expr::Bytes(Box::new(first), line, col)
                    } else if name == "trim" {
                        self.take(Kind::RParen)?;
                        Expr::Trim(Box::new(first), line, col)
                    } else if name == "slice" {
                        self.take(Kind::Comma)?;
                        let (start, start_ty) = self.expression(0, false)?;
                        if start_ty != Type::Int { return Err(self.fail("slice start must be i64")); }
                        self.take(Kind::Comma)?;
                        let (end, end_ty) = self.expression(0, false)?;
                        if end_ty != Type::Int { return Err(self.fail("slice end must be i64")); }
                        self.take(Kind::RParen)?;
                        Expr::Slice(Box::new(first), Box::new(start), Box::new(end), line, col)
                    } else if name == "split" {
                        self.take(Kind::Comma)?;
                        let (second, second_ty) = self.expression(0, false)?;
                        if !matches!(second_ty, Type::String | Type::StrView) { return Err(self.fail("split expects a string or string view separator")); }
                        self.take(Kind::RParen)?;
                        Expr::Split(Box::new(first), Box::new(second), line, col)
                    } else if matches!(name.as_str(), "contains" | "starts_with" | "ends_with") {
                        self.take(Kind::Comma)?;
                        let (second, second_ty) = self.expression(0, false)?;
                        if !matches!(second_ty, Type::String | Type::StrView) { return Err(self.fail(&format!("{name} expects a string or string view pattern"))); }
                        self.take(Kind::RParen)?;
                        match name.as_str() {
                            "contains" => Expr::Contains(Box::new(first), Box::new(second), line, col),
                            "starts_with" => Expr::StartsWith(Box::new(first), Box::new(second)),
                            _ => Expr::EndsWith(Box::new(first), Box::new(second)),
                        }
                    } else {
                        self.take(Kind::Comma)?;
                        let (second, second_ty) = self.expression(0, false)?;
                        if !matches!(second_ty, Type::String | Type::StrView) { return Err(self.fail("write_file expects a string or string view content")); }
                        self.take(Kind::RParen)?;
                        Expr::WriteFile(Box::new(first), Box::new(second), line, col)
                    };
                    let ty = match name.as_str() { "write_file" => Type::Int, "bytes" => Type::Vec, "contains" | "starts_with" | "ends_with" => Type::Bool, "split" => Type::StrVec, _ => Type::String };
                    (builtin, ty)
                } else if !self.names.contains_key(&name) && self.generics.fn_names.contains_key(&name) {
                    // Generic call: the type arguments choose the body, so they
                    // are mandatory — the language has no type inference.
                    let gid = self.generics.fn_names[&name];
                    if *self.current() != Kind::LBracket {
                        return Err(self.error_at(name_offset, &format!("'{name}' is a generic function; write its type arguments, e.g. {name}[i64](...)")));
                    }
                    let type_args = self.specialization_type_arguments(
                        name_offset, Self::MAX_GENERIC_CALL_TYPE_ARGUMENTS, Self::generic_call_type_argument,
                    )?;
                    let (id, types, ret) = self.instantiate_fn(gid, type_args, name_offset)?;
                    let args = match self.lower_committed_specialized_arguments(&types) {
                        Some(args) => args,
                        None => self.call_arguments(&name, &types)?,
                    };
                    (Expr::Call(id, args, ret), ret)
                } else if matches!(self.names.get(&name), Some((_, _, Type::Closure(_)))) && *self.current() == Kind::LParen {
                    // A closure value is a name for its generated function:
                    // the call reads the captures at the call site (ADR-0188).
                    let (expr, ty) = self.closure_call(&name, name_offset)?;
                    (expr, ty)
                } else if matches!(self.names.get(&name), Some((_, _, Type::Fn(_)))) && *self.current() == Kind::LParen {
                    // A function-typed value is called through the pointer the
                    // caller passed in (ADR-0189): the signature is interned.
                    let Some((_, _, Type::Fn(sig))) = self.names.get(&name) else { unreachable!() };
                    let sig = *sig;
                    let (params, ret) = self.ctx.fn_sig(sig);
                    let args = self.call_arguments(&name, &params)?;
                    let (id, _, _) = *self.names.get(&name).unwrap();
                    (Expr::FnCall(id, args, ret, sig), ret)
                } else if *self.current() == Kind::LParen {
                    let signature = match self.functions.get(&name).cloned() {
                        Some(signature) => signature,
                        // Not a plain function: a trait method of one of this
                        // generic's constrained parameters (or a genuine
                        // unknown name, which resolve_trait_call reports).
                        None => self.resolve_trait_call(&name, name_offset)?,
                    };
                    if !signature.public && signature.file != PRELUDE && signature.file != self.file {
                        return Err(self.error_at(name_offset, &format!("function '{name}' is private to {} (mark it 'pub' to export it)", signature.file)));
                    }
                    if (signature.extern_c || self.ctx.is_unsafe_fn(signature.id)) && self.unsafe_depth == 0 {
                        // Calling C is the unchecked operation: the compiler
                        // knows none of the callee's preconditions (M2.12
                        // slice 4, spec/unsafe.md 4.1). An `unsafe fn` is the
                        // language's own boundary with the same rule (slice 5).
                        return Err(self.error_at(name_offset, &format!("call to unsafe function '{name}' requires 'unsafe'")).with_code("E0701"));
                    }
                    let (id, types, ret) = (signature.id, signature.params.clone(), signature.ret);
                    let args = self.call_arguments(&name, &types)?;
                    (Expr::Call(id, args, ret), ret)
                } else {
                    let (id, _, ty) = *self.names.get(&name).ok_or_else(|| {
                        // A generic enum used as a value: the arguments are
                        // mandatory, so say that instead of "unknown variable".
                        if self.generics.names.contains_key(&name) {
                            return self.error_at(name_offset, &format!("'{name}' is a generic enum; write '{name}[string]::Variant'"));
                        }
                        self.error_at(name_offset, &format!("unknown variable '{name}'")).with_code("E0101")
                    })?;
                    if self.moved.contains(&id) { return Err(moved_value_hint(self.binding_label(self.error_at(name_offset, &format!("cannot use '{name}' because it was moved")).with_code("E0102"), id))); }
                    if let Type::Array(len) = ty {
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = self.expression(0, false)?;
                        self.check_array_index(index_ty.scalar(), None)?;
                        self.take(Kind::RBracket)?;
                        (Expr::Index(id, len, Box::new(index), line, col), Type::Int)
                    } else if (ty == Type::StrVec || ty == Type::StrVecRef) && *self.current() == Kind::LBracket {
                        // Reading an element borrows it; only mutation is
                        // rejected while a borrow is live (see the store paths).
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = self.expression(0, false)?;
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?;
                        (Expr::StrVecIndex(id, Box::new(index), line, col, ty == Type::StrVecRef), Type::StrView)
                    } else if (ty == Type::Vec || ty == Type::VecRef || ty == Type::VecF || ty == Type::VecFRef) && *self.current() == Kind::LBracket {
                        // Vec variables may also be used bare (moves, len);
                        // indexing is optional. Borrowed vectors are read-only.
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = self.expression(0, false)?;
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?;
                        let elem = if ty == Type::VecF || ty == Type::VecFRef { Type::F64 } else { Type::Int };
                        (Expr::VecIndex(id, Box::new(index), line, col, ty == Type::VecRef || ty == Type::VecFRef, elem), elem)
                    } else {
                        (Expr::Variable(id, ty, vline, vcol), ty)
                    }
                }
            }
            Kind::LParen => { self.pos += 1; let inner = self.expression(0, moving)?; self.take(Kind::RParen)?; inner }
            _ => return Err(self.fail("expected expression").with_code("E0011")),
        };
        // Postfix field access binds tighter than every operator.
        loop {
            // `ptr as *const U` / `ptr as *mut U` (ADR-0209 step A): the only
            // cast form. The operand must already be a pointer — value
            // conversions are to_f64/to_i64's territory — and the retag keeps
            // the address, so it is safe by itself; what the new view enables
            // is the caller's business.
            if *self.current() == Kind::As {
                if !matches!(&lhs.1, Type::Ptr(_) | Type::PtrMut(_)) {
                    // Not a pointer: leave the `as` for the enclosing borrow
                    // cast — the Amp prefix requires it right here, and its
                    // own diagnostic fires when it is missing. A non-pointer
                    // `as` at statement level surfaces as that statement's
                    // syntax error. (Check by reference: the break path must
                    // keep `lhs` intact for whatever follows the loop.)
                    break;
                }
                let (base, _) = lhs;
                self.pos += 1;
                let (mutable, pointee) = self.pointer_cast_pointee(None)?;
                let pointee_id = self.ctx.intern_pointee(pointee);
                let target = if mutable { Type::PtrMut(pointee_id) } else { Type::Ptr(pointee_id) };
                let (line, column) = self.position();
                lhs = (Expr::PtrCast(Box::new(base), mutable, pointee_id, line, column), target);
                continue;
            }
            if *self.current() != Kind::Dot { break; }
            self.pos += 1;
            let fname = self.identifier()?;
            let (base, base_ty) = lhs;
            // A raw pointer's only operation in this slice is `deref` (M2.12
            // slice 2), and only inside `unsafe` — reading through an address
            // the checker cannot validate is the unchecked operation the
            // block exists for.
            if let Type::Ptr(pointee_id) | Type::PtrMut(pointee_id) = base_ty {
                // `byte_offset(n)` (ADR-0209 step A): move the address n bytes
                // — no bounds reasoning (spec/unsafe.md 2.2), the result is
                // the same pointer type at the new address. Creating the
                // offset is unchecked address manipulation and gates on
                // `unsafe` exactly like the deref it usually precedes.
                if fname == "offset" || fname == "byte_offset" {
                    if self.unsafe_depth == 0 {
                        return Err(self.error_at(self.tokens[self.pos - 1].offset, "pointer arithmetic requires 'unsafe'"));
                    }
                    self.take(Kind::LParen)?;
                    let (count, count_ty) = self.expression(0, false)?;
                    if count_ty != Type::Int { return Err(self.fail(&format!("{fname} takes an i64 count"))); }
                    self.take(Kind::RParen)?;
                    let (line, column) = self.position();
                    let bytewise = fname == "byte_offset";
                    lhs = (Expr::PtrOffset(Box::new(base), Box::new(count), bytewise, line, column), base_ty);
                    continue;
                }
                if fname != "deref" {
                    return Err(self.fail(&format!("pointers have no method '{fname}'")));
                }
                self.check_pointer_deref(self.tokens[self.pos - 1].offset)?;
                self.take(Kind::LParen)?;
                self.take(Kind::RParen)?;
                let (line, column) = self.position();
                lhs = (Expr::DerefPtr(Box::new(base), line, column), self.ctx.ptr_pointee(pointee_id));
                continue;
            }
            // Field access works on owned structs and on read-only borrows of
            // them (ADR-0184): the borrowed form reads through the pointer and
            // emits the same `Expr::Field`, whose C emission is member access.
            // Methods resolve on the same shapes plus enums (ADR-0186).
            let (type_name, sid) = match base_ty {
                Type::Struct(sid) => (self.structs[sid].name.clone(), Some(sid)),
                Type::StructRef(sid) | Type::StructMut(sid) => (self.structs[sid].name.clone(), Some(sid)),
                // The enum's id flows to the generic-impl resolution below:
                // enum instantiations resolve methods through their
                // declaration's template, same as structs (ADR-0196).
                Type::Enum(name, eid) => (name.to_owned(), Some(eid)),
                other => return Err(self.fail(&format!("type {} has no fields or methods", other.name(self.ctx)))),
            };
            // `value.method(args)` resolves to the method's ordinary function,
            // with the receiver as argument 0 (M1.2). Nothing is added to the
            // backend: the call it builds is the one a hand-written call builds.
            if *self.current() == Kind::LParen {
                let struct_name = type_name;
                let key = format!("{struct_name}#{fname}");
                // A concrete inherent method, or one instantiation of the
                // type's generic impl block (ADR-0187): the call site cannot
                // tell them apart, and does not need to.
                let call_offset = self.tokens[self.pos].offset;
                let generic = if self.generics.inherent.contains_key(&key) {
                    None
                } else if let Some(struct_sid) = sid {
                    // Struct instantiations and enum instantiations both
                    // resolve methods through their declaration's template.
                    let origin: Option<(usize, Vec<Type>)> = match base_ty {
                        Type::Struct(_) => self.generics.struct_origin.get(&struct_sid).cloned(),
                        Type::Enum(_, _) => self.generics.enum_origin.get(&struct_sid).cloned(),
                        _ => None,
                    };
                    if let Some((gid, origin_args)) = origin {
                        // The gid indexes the table the origin came from.
                        let def_name = match base_ty {
                            Type::Enum(..) => self.generics.defs[gid].name.clone(),
                            _ => self.generics.struct_defs[gid].name.clone(),
                        };
                        if self.generics.generic_impls.contains_key(&def_name) {
                            Some(self.instantiate_impl_method(&def_name, &fname, struct_sid, gid, &origin_args, call_offset)?)
                        } else { None }
                    } else { None }
                } else { None };
                let (call_id, receiver_ty, ret, full_params) = if let Some(method) = self.generics.inherent.get(&key).cloned() {
                    if key.ends_with("#drop") {
                        return Err(self.error_at(call_offset, "explicit call to destructor 'drop' is not allowed; drop is invoked automatically when a value goes out of scope"));
                    }
                    (method.signature.id, method.receiver, method.signature.ret, method.signature.params.clone())
                } else if let Some(instance) = generic {
                    if key.ends_with("#drop") {
                        return Err(self.error_at(call_offset, "explicit call to destructor 'drop' is not allowed; drop is invoked automatically when a value goes out of scope"));
                    }
                    let (id, params, ret) = instance;
                    let def_name = match sid {
                        Some(struct_sid) => match base_ty {
                            Type::Enum(..) => self.generics.enum_origin.get(&struct_sid).map(|&(gid, _)| self.generics.defs[gid].name.clone()),
                            _ => self.generics.struct_origin.get(&struct_sid).map(|&(gid, _)| self.generics.struct_defs[gid].name.clone()),
                        }.unwrap_or_else(|| struct_name.clone()),
                        None => struct_name.clone(),
                    };
                    let has_receiver = self.generics.generic_impls.get(&def_name)
                        .and_then(|def| def.methods.iter().find(|m| m.name == fname))
                        .map(|m| m.receiver.is_some())
                        .unwrap_or(false);
                    if has_receiver {
                        let receiver = params[0];
                        (id, Some(receiver), ret, params)
                    } else {
                        (id, None, ret, params)
                    }
                } else {
                    return Err(self.no_such_method(&struct_name, &fname, "method"));
                };
                let Some(receiver_ty) = receiver_ty else {
                    return Err(self.fail(&format!("'{struct_name}::{fname}' is an associated function, so it takes no receiver; call it as '{struct_name}::{fname}(...)'")));
                };
                let signature_id = call_id;
                let signature_ret = ret;
                // A borrowed receiver (ADR-0184, ADR-0185) auto-borrows at
                // the call site: the caller keeps ownership and only a bare
                // struct variable (or a borrow itself) can be lent, since the
                // borrow must be covered by storage that outlives the call.
                if matches!(receiver_ty, Type::StructRef(_) | Type::StructMut(_)) {
                    let wants_mut = matches!(receiver_ty, Type::StructMut(_));
                    let params = full_params[1..].to_vec();
                    let mut args = self.call_arguments(&format!("{struct_name}.{fname}"), &params)?;
                    match (&base, base_ty) {
                        (Expr::Variable(id, ..), Type::Struct(_) | Type::StructMut(_)) => {
                            let id = *id;
                            if self.moved.contains(&id) {
                                return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow the receiver of '{fname}' because it was moved")).with_code("E0102"), id)));
                            }
                            if wants_mut {
                                let mutable = self.names.values().find(|slot| slot.0 == id)
                                    .map(|slot| slot.1)
                                    .ok_or_else(|| self.fail("internal: receiver variable vanished"))?;
                                if !mutable {
                                    return Err(self.fail(&format!("'{fname}' borrows its receiver mutably, but the variable was declared without 'var'")));
                                }
                                if self.is_borrowed(id) {
                                    // The callee may write any field of the
                                    // receiver, so the call releases the viewed
                                    // bytes at the call point (M2.5): with only
                                    // view borrows active the conflict defers
                                    // to the graph's ordered adjudication, like
                                    // the write it is. Same-statement call or
                                    // mut borrows keep the on-sight rejection.
                                    if !self.only_view_borrows_active(id) || !self.signature_in_release_tables(Some(signature_id)) {
                                        return Err(self.fail(&format!("cannot borrow the receiver of '{fname}' mutably while it is borrowed in this statement")));
                                    }
                                    let offset = self.tokens.get(self.pos.saturating_sub(1)).map(|t| t.offset).unwrap_or(0);
                                    let name = self.names.iter().find(|(_, (vid, _, _))| *vid == id).map(|(n, _)| n.clone()).unwrap_or_else(|| format!("v{id}"));
                                    self.deferred_write_conflicts.push((id, offset, name, "mutate", None));
                                }
                                self.mut_borrows.push((id, self.scopes.len()));
                            } else {
                                if self.mut_borrows.iter().any(|&(bid, _)| bid == id) {
                                    return Err(self.fail(&format!("cannot borrow the receiver of '{fname}' while it is mutably borrowed in this statement")));
                                }
                                if !self.is_borrowed(id) {
                                    self.call_borrows.push((id, self.scopes.len()));
                                }
                            }
                            args.insert(0, Argument::Borrow(id, matches!(base_ty, Type::Struct(_))));
                        }
                        (Expr::Variable(id, ..), Type::StructRef(_)) => {
                            // A borrow parameter forwards its pointer; the
                            // caller's own borrow already covers the reads.
                            if wants_mut {
                                return Err(self.fail(&format!("'{fname}' borrows its receiver mutably, which a read-only borrow cannot grant; call it on the owned variable")));
                            }
                            args.insert(0, Argument::Borrow(*id, false));
                        }
                        _ => return Err(self.fail(&format!("'{fname}' borrows its receiver, so the receiver must be a bare struct variable; bind the value to a variable first"))),
                    }
                    lhs = (Expr::Call(signature_id, args, signature_ret), signature_ret);
                    continue;
                }
                // The receiver is a by-value parameter, so an owned receiver
                // moves here exactly as it would in a plain call. A borrowed
                // struct has no value to move: its storage belongs to the
                // lender, so a by-value method is simply out of reach.
                if matches!(base_ty, Type::StructRef(_) | Type::StructMut(_)) {
                    return Err(self.fail(&format!("'{fname}' takes its receiver by value, but the receiver is a borrow with no value to move; call it on the owned variable")));
                }
                match &base {
                    Expr::Variable(id, ty, _, _) if ty.owned() => {
                        let id = *id;
                        // The receiver move re-homes the buffer, so a view of
                        // it dangles only if the view can still be read later
                        // (M2.5 L1): the conflict is deferred to the graph's
                        // adjudication, like the plain move it is.
                        if self.is_borrowed(id) && !self.all_borrows_are_views(id) {
                            return Err(self.fail(&format!("cannot move the receiver of '{fname}' while it is borrowed")));
                        }
                        if self.is_borrowed(id) {
                            let offset = self.tokens.get(self.pos.saturating_sub(1)).map(|t| t.offset).unwrap_or(0);
                            let name = self.names.iter().find(|(_, (vid, _, _))| *vid == id).map(|(n, _)| n.clone()).unwrap_or_else(|| format!("v{id}"));
                            self.deferred_write_conflicts.push((id, offset, name, "move", None));
                        }
                        if self.moved.contains(&id) {
                            return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot use a moved value as the receiver of '{fname}'")).with_code("E0102"), id)));
                        }
                        self.moved.insert(id);
                    }
                    Expr::Field(_, _, fty) if fty.owned() => {
                        return Err(self.fail(&format!("cannot move a field out of a struct to call '{fname}'; bind it first or pass it where a borrow works")));
                    }
                    _ => {}
                }
                let params = full_params[1..].to_vec();
                let mut args = self.call_arguments(&format!("{struct_name}.{fname}"), &params)?;
                args.insert(0, Argument::Value(base));
                lhs = (Expr::Call(signature_id, args, signature_ret), signature_ret);
                continue;
            }
            let Some(sid) = sid else {
                return Err(self.fail(&format!("enum '{}' has no fields; bind its variant payloads with 'match'", type_name)));
            };
            let fty = {
                let def = &self.structs[sid];
                let fidx = def.fields.iter().position(|(n, _)| *n == fname)
                    .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", def.name, fname)))?;
                def.fields[fidx].1
            };
            lhs = (Expr::Field(Box::new(base), fname, fty), fty);
        }
        // Move bookkeeping happens once the full postfix chain is known:
        // bare owned variables move; owned fields are borrows and can never
        // be moved out of their struct. A binary operator changes what the
        // chain means: a string `+` consumes its left operand (so an owned
        // variable is marked here, before the right side can read it again),
        // while every other operator — and the `+` itself when the operand
        // is a field or element view — only reads.
        let operator_follows = matches!(self.current(),
            Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent
            | Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq
            | Kind::Greater | Kind::GreaterEq | Kind::And | Kind::Or);
        let plus_follows = *self.current() == Kind::Plus;
        // Inside a string `+` the operands are read, so a field or an
        // element view is copied instead of rejected (concat.ae rule); an
        // owned variable is still consumed, checked and marked.
        let read_only_operand = self.plus_operand_depth > 0;
        if moving {
            let move_target = match (&lhs.0, lhs.1) {
                (_, ty) if !ty.owned() => None,
                (Expr::Variable(id, _, _, _), _) if !operator_follows || plus_follows => {
                    let vid = *id;
                    self.check_variable_move(vid)?;
                    Some(MoveKind::Var(vid))
                }
                (Expr::Field(_, _, fty), _) if fty.owned() && !operator_follows && !read_only_operand => Some(MoveKind::Field),
                (Expr::StrVecIndex(_, _, _, _, _), _) if !operator_follows && !read_only_operand => Some(MoveKind::StrVecElement),
                _ => None,
            };
            match move_target {
                Some(MoveKind::Var(id)) => { if !self.moved.contains(&id) { self.moved.insert(id); } }
                Some(MoveKind::Field) => return Err(self.fail("cannot move a field out of a struct; use it where a borrow works")),
                Some(MoveKind::StrVecElement) => return Err(self.fail("cannot move an element out of a vector; use it where a borrow works")),
                None => {}
            }
        }
        parse_infix_tail(self, lhs, min, |parser| parser.current(), Self::combine_infix)
    }

    pub(super) fn variable_move_needs_deferral(&self, id: usize) -> Option<bool> {
        // View conflicts are adjudicated at last use. Other borrows reject immediately.
        if self.is_borrowed(id) {
            self.all_borrows_are_views(id).then_some(true)
        } else {
            Some(false)
        }
    }

    pub(super) fn record_deferred_move(&mut self, id: usize, offset: usize) {
        let name = self.names.iter().find(|(_, (bound, _, _))| *bound == id)
            .map(|(name, _)| name.clone()).unwrap_or_else(|| format!("v{id}"));
        self.deferred_write_conflicts.push((id, offset, name, "move", None));
    }

    pub(super) fn check_variable_move(&mut self, id: usize) -> Result<(), ae_span::Diagnostic> {
        let defer = self.variable_move_needs_deferral(id)
            .ok_or_else(|| self.fail("cannot move a value while it is borrowed"))?;
        if defer {
            let offset = self.tokens.get(self.pos.saturating_sub(1)).map(|token| token.offset).unwrap_or(0);
            self.record_deferred_move(id, offset);
        }
        Ok(())
    }

    fn combine_infix(
        &mut self,
        lhs: (Expr, Type),
        op: Kind,
        right_power: u8,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let (line, column) = self.position();
        self.pos += 1;
        // `+` always consumes its string operands, even in borrow contexts.
        // A string `+` reads fields and vector elements rather than
        // rejecting them, so the right-hand side is parsed with the
        // plus-operand flag raised: variables are still move-checked,
        // but borrow-shaped operands are copied by the emitter.
        self.plus_operand_depth += 1;
        let parsed = self.expression(right_power, op == Kind::Plus);
        self.plus_operand_depth -= 1;
        self.lower_infix(lhs, op, parsed?, (line, column), None)
    }

    pub(super) fn check_numeric_vector_element(&self, previous: Option<Type>, actual: Type) -> Result<Type, ae_span::Diagnostic> {
        match previous {
            Some(expected) if expected == actual => Ok(actual),
            Some(expected) => Err(self.fail(&format!("vector elements must all be {}; {} does not convert implicitly", expected.name(self.ctx), actual.name(self.ctx)))),
            None if matches!(actual, Type::Int | Type::F64) => Ok(actual),
            None => Err(self.fail(&format!("vector elements must be i64 or f64, found {}", actual.name(self.ctx)))),
        }
    }

    pub(super) fn check_address_binding(&self, id: usize, mutable: bool) -> Result<(), ae_span::Diagnostic> {
        if mutable {
            let binding = self.names.values().find(|slot| slot.0 == id)
                .ok_or_else(|| self.fail("internal: pointer operand vanished"))?;
            if !binding.1 {
                return Err(self.fail("cannot take a mutable pointer to an immutable binding; declare it with 'var'"));
            }
        }
        Ok(())
    }

    pub(super) fn lower_address_cast(
        &mut self, operand: Expr, ty: Type, pointee: Type, mutable: bool, (line, column): (usize, usize),
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        if pointee != ty {
            return Err(self.fail(&format!("cannot cast &{} to *{}{}; reinterpret casts do not exist yet", ty.name(self.ctx), if mutable { "mut " } else { "const " }, pointee.name(self.ctx))));
        }
        // Raw pointer borrows are untracked, so moving an owned pointee
        // would let safe code leave the pointer dangling.
        if ty.owned() {
            return Err(self.fail("a pointer to an owned value is not supported yet; its borrow is not tracked, so safe code could dangle it by moving the value"));
        }
        let id = self.ctx.intern_pointee(ty);
        let pointer = if mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
        Ok((Expr::BorrowAs(Box::new(operand), mutable, line, column), pointer))
    }

    pub(super) fn check_array_index(
        &mut self,
        scalar: Option<ae_ty::Scalar>,
        end: Option<usize>,
    ) -> Result<(), ae_span::Diagnostic> {
        if !ae_ty::valid_array_index(scalar) {
            if let Some(end) = end {
                self.seek_scalar_offset(end);
            }
            return Err(self.fail("array index must be an integer").with_code("E0201"));
        }
        Ok(())
    }

    fn operator_error(&self, error: ae_ty::OperatorError) -> ae_span::Diagnostic {
        let diagnostic = self.fail(error.message());
        if let Some(code) = error.code() { diagnostic.with_code(code) } else { diagnostic }
    }

    fn operator_type(
        &self,
        fact: Option<ae_resolve::resolve::ExpressionType>,
        check: impl FnOnce() -> Result<ae_ty::Scalar, ae_ty::OperatorError>,
    ) -> Result<Type, ae_span::Diagnostic> {
        use ae_resolve::resolve::ExpressionType;
        let result = match fact {
            Some(ExpressionType::Checked(scalar)) => Ok(scalar),
            Some(ExpressionType::Rejected(error)) => Err(error),
            Some(ExpressionType::Deferred) | None => check(),
        };
        result.map(|scalar| scalar.ty()).map_err(|error| self.operator_error(error))
    }

    pub(super) fn lower_prefix(
        &self,
        operator: Kind,
        (operand, ty): (Expr, Type),
        (line, column): (usize, usize),
        fact: Option<ae_resolve::resolve::ExpressionType>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let ty = self.operator_type(fact, || ae_ty::prefix(&operator, ty.scalar()))?;
        let value = if operator == Kind::Minus {
            Expr::Neg(Box::new(operand), line, column)
        } else {
            Expr::Not(Box::new(operand))
        };
        Ok((value, ty))
    }

    pub(super) fn lower_infix(
        &mut self,
        lhs: (Expr, Type),
        op: Kind,
        (rhs, rhs_ty): (Expr, Type),
        (line, column): (usize, usize),
        fact: Option<ae_resolve::resolve::ExpressionType>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let (lhs_expr, lhs_ty) = lhs;
        let stringlike = |ty: Type| matches!(ty, Type::String | Type::StrView);
        if matches!(op, Kind::EqEq | Kind::NotEq) && stringlike(lhs_ty) && stringlike(rhs_ty) {
            return Ok((Expr::StrEq(Box::new(lhs_expr), Box::new(rhs), op == Kind::NotEq), Type::Bool));
        }
        let ty = if op == Kind::Plus && (stringlike(lhs_ty) || stringlike(rhs_ty)) {
            if !stringlike(lhs_ty) || !stringlike(rhs_ty) {
                return Err(self.fail("'+' requires two strings, two string views, or two integers"));
            }
            // Concatenation consumes owned variables and copies borrowed views.
            if let Expr::Variable(id, Type::String, ..) = &lhs_expr {
                self.moved.insert(*id);
            }
            Type::String
        } else {
            self.operator_type(fact, || {
                ae_ty::infix(&op, lhs_ty.scalar(), rhs_ty.scalar())
            })?
        };
        Ok((Expr::Binary(Box::new(lhs_expr), op, Box::new(rhs), line, column, ty), ty))
    }
}

