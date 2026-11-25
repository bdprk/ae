//! Lowering module: builtin (P1.4 parser split).
use super::*;
use super::parser::moved_value_hint;
use super::route_marks::resolved_builtin;

/// The runtime-object intrinsic surface frozen by ADR-0224. Keep this list
/// in sync with `docs/architecture/intrinsic-allowlist.txt`;
/// `tests/intrinsic_freeze.rs` reads this line.
pub(super) const RUNTIME_OBJECT_INTRINSICS: &[&str] = &["shared_new", "shared_get", "mutex_new", "mutex_lock", "chan_new", "chan_send", "chan_try_send", "chan_recv", "chan_select", "exec_new", "exec_run", "scope_new", "pool_new", "pool_join"];

impl<'a> Parser<'a> {
    /// The pure value builtins, lowered with the same `Expr` variants and the
    /// same argument rules the typed route applies (positions included). The
    /// resolved Call arm dispatches here for `resolved_builtin` names; no
    /// `Argument::Borrow` machinery is involved because every one of these
    /// builtins only reads its arguments.
    pub(super) fn lower_resolved_builtin(
        &mut self,
        name: &str,
        expression: &ae_syntax::expression::Expression,
        callee: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        debug_assert!(RUNTIME_OBJECT_INTRINSICS.iter().all(|name| resolved_builtin(name)));
        use ae_syntax::expression::ExpressionKind;
        (|| -> Result<(Expr, Type), ae_span::Diagnostic> {
            self.seek_scalar_offset(callee.range.start);
            let (line, column) = self.position();
            match name {
                "len" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    // The typed route's fast path: a bare local's array length
                    // is a compile-time constant, and a vector or map length
                    // borrows the slot. Either way the variable must not be
                    // moved.
                    if let ExpressionKind::Atom(Kind::Ident(argument_name)) = &argument.kind
                        && let Some(&def) = self.resolved_expressions().scalar_reads.get(&argument.range.start)
                        && let Some(&(slot, ty)) = slots.get(&def)
                    {
                                self.seek_scalar_offset(argument.range.start);
                                if self.moved.contains(&slot) {
                                    return Err(moved_value_hint(self.binding_label(
                                        self.fail(&format!("cannot use '{argument_name}' because it was moved")).with_code("E0102"), slot)));
                                }
                                if let Type::ArrayOf(_, n) = ty {
                                    self.seek_scalar_offset(expression.range.end);
                                    return Ok((Expr::Number(n as i64), Type::Int(IntTy::I64)));
                                }
                                if matches!(ty, Type::Vector(_) | Type::VectorRef(_) | Type::MapOf(_)) {
                                    // The typed fast path stamps the operand with
                                    // the `len` call's position, not the variable's.
                                    let value = Expr::Variable(slot, ty, line, column);
                                    self.seek_scalar_offset(expression.range.end);
                                    return Ok((Expr::Len(Box::new(value), line, column), Type::Int(IntTy::I64)));
                                }
                    }
                    let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                    if ty == Type::Tokens {
                        // The typed route consumes len's `)` before it checks
                        // the operand type, so the cursor is one token past it.
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail("a token cursor has no len: counting would consume the stream, so use split(text, sep) when the count is needed"));
                    }
                    if !matches!(ty, Type::String | Type::StrView | Type::Vector(_) | Type::VectorRef(_) | Type::MapOf(_)) {
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail("len expects a string, vec, map, or array variable"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Len(Box::new(value), line, column), Type::Int(IntTy::I64)))
                }
                "to_string" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    let value = match actual {
                        Type::Int(_) | Type::Char => Expr::ToString(Box::new(value), line, column),
                        Type::Float(_) => Expr::FloatToString(Box::new(value), line, column),
                        _ => {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail("to_string expects an i64 or f64"));
                        }
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, Type::String))
                }
                "to_f64" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    if actual != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(argument.range.end);
                        return Err(self.fail("to_f64 expects an i64"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ToF64(Box::new(value), line, column), Type::Float(FloatTy::F64)))
                }
                "trim" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    if !matches!(actual, Type::String | Type::StrView) {
                        self.seek_scalar_offset(argument.range.end);
                        return Err(self.fail("trim expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Trim(Box::new(value), line, column), Type::String))
                }
                "slice" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (text, start, end) = (&arguments[0], &arguments[1], &arguments[2]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail("slice expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(start.range.start);
                    let (from, from_ty) = self.lower_scalar_body_expression(start, slots)?;
                    if from_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(start.range.end);
                        return Err(self.fail("slice start must be i64"));
                    }
                    self.seek_scalar_offset(end.range.start);
                    let (to, to_ty) = self.lower_scalar_body_expression(end, slots)?;
                    if to_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(end.range.end);
                        return Err(self.fail("slice end must be i64"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Slice(Box::new(first), Box::new(from), Box::new(to), line, column), Type::String))
                }
                "split" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, separator) = (&arguments[0], &arguments[1]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail("split expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(separator.range.start);
                    let (second, second_ty) = self.lower_scalar_body_expression(separator, slots)?;
                    if !matches!(second_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(separator.range.end);
                        return Err(self.fail("split expects a string or string view separator"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let ty = Type::Vector(self.ctx.intern_vector_elem(Type::String));
                    Ok((Expr::Split(Box::new(first), Box::new(second), line, column), ty))
                }
                "contains" | "starts_with" | "ends_with" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, pattern) = (&arguments[0], &arguments[1]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view argument")));
                    }
                    self.seek_scalar_offset(pattern.range.start);
                    let (second, second_ty) = self.lower_scalar_body_expression(pattern, slots)?;
                    if !matches!(second_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(pattern.range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view pattern")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let value = match name {
                        "contains" => Expr::Contains(Box::new(first), Box::new(second), line, column),
                        "starts_with" => Expr::StartsWith(Box::new(first), Box::new(second)),
                        _ => Expr::EndsWith(Box::new(first), Box::new(second)),
                    };
                    Ok((value, Type::Bool))
                }
                "map_new" | "f64map_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    let (kind, val_ty) = if name == "map_new" {
                        (MapKind::Int, Type::Int(IntTy::I64))
                    } else {
                        (MapKind::Float, Type::Float(FloatTy::F64))
                    };
                    let ty = Type::MapOf(self.ctx.intern_map_types(Type::String, val_ty));
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MapNew(line, column, kind), ty))
                }
                "map_get" | "map_has" | "map_remove" | "map_inc" | "keys" => {
                    let expected = if matches!(name, "map_inc") { 3 } else if matches!(name, "keys") { 1 } else { 2 };
                    self.expect_resolved_builtin_arity(expression, arguments, expected)?;
                    // The map argument is a bare map variable, or a map field of
                    // a struct *variable* (the borrow stays statement-scoped).
                    let (map, map_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let kind = map_kind_of(map_ty, self.ctx);
                    let place = match (&map, map_ty) {
                        (Expr::Variable(id, inner, ..), t) if *inner == t && matches!(t, Type::MapOf(_)) => MapPlace::Var(*id, kind),
                        (Expr::Field(base, fname, t), _) if matches!(*t, Type::MapOf(_)) => match **base {
                            Expr::Variable(id, ..) => MapPlace::Field(id, fname.clone(), kind),
                            _ => {
                                self.seek_scalar_offset(arguments[0].range.end);
                                return Err(self.fail(&format!("{name} needs a map variable or a field of one")));
                            }
                        },
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail(&format!("{name} expects a map variable or a field of one")));
                        }
                    };
                    if self.moved.contains(&place.slot()) {
                        return Err(self.binding_label(self.fail(&format!("cannot use '{name}' on a moved map")).with_code("E0102"), place.slot()));
                    }
                    if matches!(name, "keys") {
                        self.seek_scalar_offset(expression.range.end);
                        let ty = Type::Vector(self.ctx.intern_vector_elem(Type::String));
                        return Ok((Expr::MapKeys(place, line, column), ty));
                    }
                    let (key, key_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(key_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        let message = if matches!(name, "map_inc") { "map_inc expects a string key".to_owned() } else { format!("{name} expects a string key") };
                        return Err(self.fail(&message));
                    }
                    if matches!(name, "map_inc") {
                        let (delta, delta_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                        if delta_ty != map_element_ty(kind) {
                            self.seek_scalar_offset(arguments[2].range.end);
                            return Err(self.fail(&format!("map_inc expects a {} delta", map_element_ty(kind).name(self.ctx))));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::MapInc(place, Box::new(key), Box::new(delta), line, column), map_element_ty(kind)));
                    }
                    let ty = match name {
                        "map_get" => self.prelude_type(if kind == MapKind::Float { "FloatResult" } else { "IntResult" }),
                        _ => Type::Bool,
                    };
                    let value = match name {
                        "map_get" => Expr::MapGet(place, Box::new(key), ty, line, column),
                        "map_has" => Expr::MapHas(place, Box::new(key), line, column),
                        _ => Expr::MapRemove(place, Box::new(key), line, column),
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                "cell_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (seed, seed_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if seed_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_new expects an i64 seed"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicNew(Box::new(seed)), Type::Atomic))
                }
                "cell_load" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_load expects an atomic handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicLoad(Box::new(handle)), Type::Int(IntTy::I64)))
                }
                "cell_add" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_add expects an atomic handle"));
                    }
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if value_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("cell_add expects an i64 delta"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicAdd(Box::new(handle), Box::new(value)), Type::Int(IntTy::I64)))
                }
                "cell_cas" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_cas expects an atomic handle"));
                    }
                    let (expect, expect_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if expect_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("cell_cas expects an i64 expected value"));
                    }
                    let (new, new_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                    if new_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[2].range.end);
                        return Err(self.fail("cell_cas expects an i64 new value"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicCas(Box::new(handle), Box::new(expect), Box::new(new)), Type::Bool))
                }
                "shared_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if matches!(value_ty, Type::Shared(_)) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("shared_new cannot wrap another shared handle (no handle towers)"));
                    }
                    if !matches!(value_ty, Type::Int(IntTy::I64) | Type::Bool | Type::Float(FloatTy::F64) | Type::Atomic) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("shared_new in v1 expects a copy-semantics value (i64, bool, f64, or atomic)"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let sid = self.ctx.intern_shared(value_ty);
                    Ok((Expr::SharedNew(Box::new(value), sid), Type::Shared(sid)))
                }
                "shared_get" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let sid = match handle_ty {
                        Type::Shared(sid) => sid,
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("shared_get expects a shared handle"));
                        }
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::SharedGet(Box::new(handle), sid), self.ctx.shared_types_get(sid)))
                }
                "mutex_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MutexNew, Type::Mutex))
                }
                "mutex_lock" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Mutex {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("mutex_lock expects a mutex handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MutexLock(Box::new(handle)), Type::Guard))
                }
                "chan_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (capacity, capacity_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if capacity_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_new expects an i64 capacity"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanNew(Box::new(capacity)), Type::Chan))
                }
                "chan_send" | "chan_try_send" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (channel, channel_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if channel_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a chan handle")));
                    }
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if value_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects an i64 value")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok(if name == "chan_send" {
                        (Expr::ChanSend(Box::new(channel), Box::new(value)), Type::Bool)
                    } else {
                        (Expr::ChanTrySend(Box::new(channel), Box::new(value)), Type::Bool)
                    })
                }
                "chan_recv" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (channel, channel_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if channel_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_recv expects a chan handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanRecv(Box::new(channel)), Type::Int(IntTy::I64)))
                }
                "chan_select" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (first, first_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if first_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_select expects chan handles"));
                    }
                    let (second, second_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if second_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("chan_select expects chan handles"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanSelect(Box::new(first), Box::new(second)), Type::Int(IntTy::I64)))
                }
                "exec_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ExecNew, Type::Exec))
                }
                "exec_run" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Exec {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("exec_run expects an exec handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ExecRun(Box::new(handle)), Type::Int(IntTy::I64)))
                }
                "scope_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ScopeNew, Type::TaskScope))
                }
                "pool_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (workers, workers_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if workers_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("pool_new expects an i64 worker count"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PoolNew(Box::new(workers)), Type::Pool))
                }
                "pool_join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Pool {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("pool_join expects a pool handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PoolJoin(Box::new(handle)), Type::Int(IntTy::I64)))
                }
                "c_size_of" | "c_align_of" | "c_offset_of" => {
                    // Only the bracketed form exists; the bracket path is
                    // intercepted before this arm, so a plain call lands here.
                    Err(self.fail(&format!("{name} is a layout query; write {name}[Type](..)")))
                }
                "spawn" => {
                    // Cursor after the name: the typed spawn helper parses the
                    // callable, moves its captures, and interns the thread.
                    self.pos += 1;
                    let (value, ty) = self.spawn_expr_with(line, column, arguments.first(), slots)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                "wait" => {
                    let Some(handle) = arguments.first() else {
                        self.seek_scalar_offset(expression.range.end);
                        self.pos = self.pos.saturating_sub(1);
                        return Err(self.fail("expected expression").with_code("E0011"));
                    };
                    let (value, handle_ty) = self.lower_scalar_body_expression(handle, slots)?;
                    // `wait` consumes the handle: the typed reader parsed it
                    // with the moving flag, so the same slot bookkeeping
                    // applies here.
                    if self.is_owned(handle_ty)
                        && let Expr::Variable(slot, ..) = &value {
                            self.check_variable_move(*slot)?;
                            self.moved.insert(*slot);
                        }
                    self.seek_scalar_offset(expression.range.end);
                    let Type::Thread(tid) = handle_ty else {
                        return Err(self.fail("wait expects a thread handle"));
                    };
                    let ret = self.generics.threads[tid];
                    Ok((Expr::Wait(Box::new(value), ret, line, column), ret))
                }
                "now_ms" | "now_s" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "now_ms" { Expr::NowMs } else { Expr::NowS }, Type::Int(IntTy::I64)))
                }
                "args" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    let ty = Type::Vector(self.ctx.intern_vector_elem(Type::String));
                    Ok((Expr::Args(line, column), ty))
                }
                "read_stdin" | "cwd" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    let ty = self.prelude_type("ReadResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "read_stdin" { Expr::ReadStdin(ty, line, column) } else { Expr::Cwd(ty, line, column) }, ty))
                }
                "read_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("read_file expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ReadFile(Box::new(path), line, column), Type::String))
                }
                "write_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("write_file expects a string or string view argument"));
                    }
                    let (content, content_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(content_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("write_file expects a string or string view content"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::WriteFile(Box::new(path), Box::new(content), line, column), Type::Int(IntTy::I64)))
                }
                "path_exists" | "path_is_dir" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "path_exists" {
                        Expr::PathExists(Box::new(path), line, column)
                    } else {
                        Expr::PathIsDir(Box::new(path), line, column)
                    }, Type::Bool))
                }
                "path_join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (directory, directory_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(directory_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("path_join expects a string directory"));
                    }
                    let (part, part_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(part_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("path_join expects a string name"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PathJoin(Box::new(directory), Box::new(part), line, column), Type::String))
                }
                "path_name" | "path_parent" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "path_name" {
                        Expr::PathName(Box::new(path), line, column)
                    } else {
                        Expr::PathParent(Box::new(path), line, column)
                    }, Type::String))
                }
                "try_read_file" | "try_list_dir" | "try_file_info" | "env" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (ty, message) = match name {
                        "try_read_file" => (self.prelude_type("ReadResult"), "try_read_file expects a string path"),
                        "try_list_dir" => (self.prelude_type("DirResult"), "try_list_dir expects a string path"),
                        "try_file_info" => (self.prelude_type("FileResult"), "try_file_info expects a string path"),
                        _ => (self.prelude_type("ReadResult"), "env expects a string variable name"),
                    };
                    let (operand, operand_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(operand_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(message));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((match name {
                        "try_read_file" => Expr::TryReadFile(Box::new(operand), ty, line, column),
                        "try_list_dir" => Expr::TryListDir(Box::new(operand), ty, line, column),
                        "try_file_info" => Expr::TryFileInfo(Box::new(operand), ty, line, column),
                        _ => Expr::Env(Box::new(operand), ty, line, column),
                    }, ty))
                }
                "try_write_file" | "try_append_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("WriteResult");
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    let (content, content_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(content_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view content")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "try_write_file" {
                        Expr::TryWriteFile(Box::new(path), Box::new(content), ty, line, column)
                    } else {
                        Expr::TryAppendFile(Box::new(path), Box::new(content), ty, line, column)
                    }, ty))
                }
                "try_copy_file" | "try_rename_path" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("StatusResult");
                    let (from, from_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(from_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string source path")));
                    }
                    let (to, to_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(to_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string destination path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "try_copy_file" {
                        Expr::TryCopyFile(Box::new(from), Box::new(to), ty, line, column)
                    } else {
                        Expr::TryRenamePath(Box::new(from), Box::new(to), ty, line, column)
                    }, ty))
                }
                "try_remove_file" | "try_remove_dir" | "try_create_dir" | "set_cwd" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let ty = self.prelude_type("StatusResult");
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((match name {
                        "try_remove_file" => Expr::TryRemoveFile(Box::new(path), ty, line, column),
                        "try_remove_dir" => Expr::TryRemoveDir(Box::new(path), ty, line, column),
                        "try_create_dir" => Expr::TryCreateDir(Box::new(path), ty, line, column),
                        _ => Expr::SetCwd(Box::new(path), ty, line, column),
                    }, ty))
                }
                _ => self.lower_resolved_builtin_surface(name, expression, callee, arguments, slots),
            }
        })()
    }

    /// The typed route's own arity behaviour for a builtin that parses
    /// `(` expression (`(, expression)*`) `)`: a missing argument produces the
    /// Pratt parser's `expected expression` (E0011) at the closing
    /// parenthesis, and an extra one produces `take(RParen)`'s `expected
    /// RParen, found Comma` (E0010) at the comma before it. The pinned
    /// diagnostics stay identical because the same rules are applied.
    pub(super) fn expect_resolved_builtin_arity(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        expected: usize,
    ) -> Result<(), ae_span::Diagnostic> {
        if arguments.len() < expected {
            self.seek_scalar_offset(expression.range.end);
            self.pos = self.pos.saturating_sub(1);
            return Err(self.fail("expected expression").with_code("E0011"));
        }
        if arguments.len() > expected {
            self.seek_scalar_offset(arguments[expected].range.start);
            self.pos = self.pos.saturating_sub(1);
            self.take(Kind::RParen)?;
        }
        Ok(())
    }

}
