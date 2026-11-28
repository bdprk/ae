//! Expression parsing and type checking.
use super::*;
use super::closures::SpawnedThread;
use super::parser::moved_value_hint;

impl Parser<'_> {

    // A compile-time integer constant as an expression: the layout queries
    // evaluate in the parser and reach every later stage as plain numbers.
    fn value_number(&self, n: i64) -> (Expr, Type) {
        (Expr::Number(n), Type::Int(IntTy::I64))
    }


    /// `c_size_of[T]()` / `c_align_of[T]()` / `c_offset_of[T]("field")` — cursor
    /// on the `[`. Compile-time layout constants over a `#[repr(C)]` struct
    /// (M2.12 U8), so a layout regression fails a test rather than an FFI
    /// session. Shared by the typed dispatch and the resolved body route.
    #[inline(never)]
    pub(super) fn c_layout_expr(&mut self, name: &str, field: Option<&ae_syntax::expression::Expression>) -> Result<(Expr, Type), ae_span::Diagnostic> {
        self.pos += 1;
        // The spelling may nest brackets (`vec[i64]`, `map[string, f64]`):
        // consume through the matching close and answer as text, the same
        // way the scalar names are matched below.
        let spelling_start = self.tokens[self.pos].offset;
        let mut depth = 0usize;
        loop {
            match self.tokens[self.pos].kind {
                Kind::LBracket => {
                    depth += 1;
                    self.pos += 1;
                }
                Kind::RBracket => {
                    if depth == 0 {
                        // The query's own closing bracket: leave it for the
                        // take() calls downstream, which consume it.
                        break;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                Kind::Eof => return Err(self.fail("expected ']' after the layout query's type spelling")),
                _ => self.pos += 1,
            }
        }
        let type_name = self.source[spelling_start..self.tokens[self.pos].offset].trim().to_owned();
        // A type parameter inside a generic body resolves through the
        // substitution table first (ADR-0209 step A): at instantiation the
        // body is re-parsed with T bound to the concrete struct, so
        // `c_size_of[T]()` must see through it. An unsubstituted unknown name
        // keeps today's error.
        let scalar = match self.subst.get(&type_name) {
            Some(ty) => ty.scalar_c_layout(),
            None => match type_name.as_str() {
                "i64" => Type::Int(IntTy::I64).scalar_c_layout(),
                "f64" => Type::Float(FloatTy::F64).scalar_c_layout(),
                "bool" => Type::Bool.scalar_c_layout(),
                _ => None,
            },
        };
        // `string` has a promised layout too (ADR-0245): the pure runtime's
        // `ae_str {ptr, len, owned}` — 24 bytes at pointer alignment. The
        // legacy backend stores 16 and over-allocates in the slot until its
        // retirement; the constant is one number for both. The container
        // layouts are the same deal: the pure runtime's `ae_vec` (five
        // pointer-or-int fields, 40 bytes) and the legacy `ae_strvec` agree
        // on the size, so a heap slot is opaque 40 bytes to the author.
        let owned_layout = match self.subst.get(&type_name) {
            Some(Type::String) | Some(Type::StrView) => Some((24, 8)),
            Some(Type::Vector(_)) | Some(Type::VectorRef(_)) => Some((40, 8)),
            Some(_) => None,
            None => match type_name.as_str() {
                "string" => Some((24, 8)),
                s if s.starts_with("vec[") => Some((40, 8)),
                _ => None,
            },
        };
        if name != "c_offset_of" && let Some((size, align)) = owned_layout.or(scalar) {
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
                    // The field argument arrives as the shared call node: the
                    // route no longer token-parses it.
                    let Some(field) = field else {
                        return Err(self.fail("c_offset_of needs the shared field argument"));
                    };
                    let ae_syntax::expression::ExpressionKind::Atom(Kind::Text(fname)) = &field.kind else {
                        return Err(self.fail("offset_of's field is written as a string literal"));
                    };
                    self.seek_scalar_offset(field.range.end);
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
                    if !matches!(ty, Type::Int(IntTy::I64) | Type::Bool | Type::Float(FloatTy::F64) | Type::String | Type::Vector(_) | Type::MapOf(_) | Type::Struct(_) | Type::Enum(..) | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool) {
                        return Err(self.fail("spawn moves its captures into the thread; this capture's storage cannot cross"));
                    }
                    if self.moved.contains(outer_id) {
                        return Err(moved_value_hint(self.error_at(at, "cannot spawn a closure that captures a moved variable").with_code("E0102")));
                    }
                    if self.is_borrowed(*outer_id) {
                        return Err(self.fail("cannot spawn while one of the closure's captured variables is borrowed"));
                    }
                    if self.is_owned(*ty) { self.moved.insert(*outer_id); }
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
        self.parameter_type()
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


    pub(super) fn check_numeric_vector_element(&self, previous: Option<Type>, actual: Type) -> Result<Type, ae_span::Diagnostic> {
        // A string view element (`slice_view(..)` inside `vec[..]`, a field
        // read) is copied into the vector, so the literal's element type is
        // the owning `string`.
        let actual = if actual == Type::StrView { Type::String } else { actual };
        match previous {
            Some(expected) if expected == actual => Ok(actual),
            Some(expected) => Err(self.fail(&format!("vector elements must all be {}; {} does not convert implicitly", expected.name(self.ctx), actual.name(self.ctx)))),
            None if actual == Type::String
                || self.is_array_element_type(actual)
                || matches!(actual, Type::Vector(_))
                || matches!(actual, Type::Struct(sid) if !self.structs[sid].is_copy) => Ok(actual),
            None => Err(self.fail(&format!("vector literal elements must be a value type, found {}", actual.name(self.ctx)))),
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
        if self.is_owned(ty) {
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

