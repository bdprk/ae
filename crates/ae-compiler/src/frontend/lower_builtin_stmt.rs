//! Lowering module: builtin stmt (P1.4 parser split).
use super::*;
use super::parser::moved_value_hint;
use super::route_body_stmt::*;

impl<'a> Parser<'a> {
    pub(super) fn lower_handle_builtin_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let (builtin, call) = handle_statement_builtin(statement).expect("handle builtin eligibility");
        // The spawn statement forms share the typed parser's helper (cursor on
        // the name); it consumes the arguments and the semicolon.
        if matches!(builtin, "task_spawn" | "scope_spawn" | "pool_spawn") {
            self.seek_scalar_offset(call.range.start);
            return self.spawn_statement(builtin);
        }
        let ExpressionKind::Call { callee, arguments } = &call.kind else { unreachable!("handle builtin call") };
        self.seek_scalar_offset(callee.range.start);
        if builtin == "chan_close" {
            let Some(argument) = arguments.first() else {
                self.seek_scalar_offset(call.range.end);
                self.pos = self.pos.saturating_sub(1);
                return Err(self.fail("expected expression").with_code("E0011"));
            };
            let (handle, handle_ty) = self.lower_scalar_body_expression(argument, slots)?;
            if handle_ty != Type::Chan {
                self.seek_scalar_offset(argument.range.end);
                return Err(self.fail("chan_close expects a chan handle"));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(Statement::ChanClose(Box::new(handle)));
        }
        if arguments.len() < 2 {
            self.seek_scalar_offset(call.range.end);
            self.pos = self.pos.saturating_sub(1);
            return Err(self.fail("expected expression").with_code("E0011"));
        }
        let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
        if builtin == "cell_store" {
            if handle_ty != Type::Atomic {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail("cell_store expects an atomic handle"));
            }
            let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
            if value_ty != Type::Int {
                self.seek_scalar_offset(arguments[1].range.end);
                return Err(self.fail("cell_store expects an i64 value"));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(Statement::AtomicStore(Box::new(handle), Box::new(value)));
        }
        // `shared_set(m, v);` — the typed route's seq_cst store, guarded by a
        // live mutex guard from `mutex_lock(...)` in the same body.
        let sid = match handle_ty {
            Type::Shared(sid) => sid,
            _ => {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail("shared_set expects a shared handle"));
            }
        };
        let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
        let expected = self.ctx.shared_types_get(sid);
        if value_ty != expected {
            self.seek_scalar_offset(arguments[1].range.end);
            return Err(self.fail(&format!("shared_set expects {}, found {}", expected.name(self.ctx), value_ty.name(self.ctx))));
        }
        self.seek_scalar_offset(call.range.end);
        self.take(Kind::Semi)?;
        if !self.live_owned().iter().any(|(_, ty)| *ty == Type::Guard) {
            return Err(self.fail("shared_set requires a live mutex guard in scope; call mutex_lock(...) first"));
        }
        Ok(Statement::SharedSet(Box::new(handle), Box::new(value), sid))
    }

    pub(super) fn lower_vector_builtin_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let resolved_cell = Rc::clone(&self.resolved_expressions);
        let resolved = resolved_cell.get_or_init(|| self.build_resolved_expressions());
        let (builtin, value) = vector_statement_builtin(statement, resolved).expect("vector builtin eligibility");
        let ae_syntax::statement::StatementKind::Expr { value: call, .. } = &statement.kind else { unreachable!("vector builtin statement") };
        let ExpressionKind::Call { callee, arguments } = &call.kind else { unreachable!("vector builtin call") };
        let ExpressionKind::Atom(Kind::Ident(target)) = &arguments[0].kind else { unreachable!("vector builtin target") };
        self.seek_scalar_offset(callee.range.start);
        let (line, column) = self.position();
        let def = resolved.scalar_reads[&arguments[0].range.start];
        let (slot, ty) = slots[&def];
        if self.moved.contains(&slot) {
            return Err(moved_value_hint(self.binding_label(
                self.fail(&format!("cannot use '{target}' because it was moved")).with_code("E0102"), slot)));
        }
        if !matches!(ty, Type::Vec | Type::StrVec | Type::VecF) {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.fail(&format!("{builtin} expects a vec, found {}", ty.name(self.ctx))));
        }
        if self.is_borrowed(slot) {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.fail(&format!("cannot modify '{target}' while it is borrowed")));
        }
        // The immutability diagnostic is emitted per branch: the typed route
        // reports a `push` target at the value's token (the shared expression
        // path lowers the value first) and the others at the closing paren.
        // Ordering happens in place (qsort for i64, byte order for strings);
        // f64 has no total order, so sort/reverse reject it.
        if matches!(builtin, "sort" | "reverse") {
            if !self.names[target].1 {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.immutable_binding_error(
                    self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
            }
            if ty == Type::VecF {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail(&format!("{builtin} is not supported for vec[f64]; NaN has no total order")));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            let strings = ty == Type::StrVec;
            return Ok(if builtin == "sort" { Statement::Sort(slot, strings, line, column) } else { Statement::Reverse(slot, strings, line, column) });
        }
        if builtin == "push" {
            // The immutability diagnostic points just past the target name
            // (the `,`), the token the typed parser's cursor ends on.
            if !self.names[target].1 {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.immutable_binding_error(
                    self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
            }
            let (lowered, value_ty) = self.lower_scalar_body_expression(value, slots)?;
            let ok = match ty {
                Type::Vec => value_ty == Type::Int,
                Type::VecF => value_ty == Type::F64,
                _ => matches!(value_ty, Type::String | Type::StrView),
            };
            if !ok {
                self.seek_scalar_offset(value.range.end);
                return Err(self.fail(&format!("vector elements must be {}", match ty { Type::Vec => "i64", Type::VecF => "f64", _ => "string" })));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(if ty == Type::StrVec {
                Statement::StrVecPush(slot, lowered, line, column, value_ty == Type::StrView)
            } else if ty == Type::VecF {
                Statement::VecFPush(slot, lowered, line, column)
            } else {
                Statement::VecPush(slot, lowered, line, column)
            });
        }
        if !self.names[target].1 {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.immutable_binding_error(
                self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
        }
        self.seek_scalar_offset(call.range.end);
        self.take(Kind::Semi)?;
        Ok(if ty == Type::StrVec {
            Statement::StrVecPop(slot, line, column)
        } else if ty == Type::VecF {
            Statement::VecFPop(slot, line, column)
        } else {
            Statement::VecPop(slot, line, column)
        })
    }

}
