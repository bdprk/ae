//! Lowering module: loop (P1.4 parser split).
use super::*;

impl<'a> Parser<'a> {
    pub(super) fn lower_scalar_range(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        statements: &mut Vec<Statement>,
    ) -> Result<(), ae_span::Diagnostic> {
        let ae_syntax::statement::StatementKind::ForRange { binding, binding_range, start, end, body } = &statement.kind else {
            unreachable!("scalar range eligibility");
        };
        self.seek_scalar_offset(binding_range.end);
        if RESERVED.contains(&binding.as_str()) {
            return Err(self.fail(&format!("'{binding}' is a built-in name and cannot be a loop variable")));
        }
        if self.names.contains_key(binding) {
            return Err(self.duplicate_binding_error(binding));
        }
        let start = self.lower_scalar_range_bound(start, slots)?;
        let end = self.lower_scalar_range_bound(end, slots)?;
        if self.loop_depth >= 64 {
            return Err(self.fail("loop nesting limit exceeded (64)"));
        }
        let (line, column) = self.position();
        let end_slot = self.next_var;
        let slot = end_slot + 1;
        self.next_var += 2;
        let label = self.next_label;
        self.next_label += 1;
        let def = self.resolved_definition(
            self.resolved_expressions().scalar_definitions.get(&binding_range.start),
            binding_range.start, binding)?;
        slots.insert(def, (slot, Type::Int(IntTy::I64)));
        self.names.insert(binding.clone(), (slot, false, Type::Int(IntTy::I64)));
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        let body = self.lower_scalar_loop_body(body, slots, Some(label));
        slots.remove(&def);
        self.names.remove(binding);
        let body = body?;
        // The body runs again: a value moved out of an outer variable must be
        // re-initialized before the body ends (the typed route's ADR-20 rule,
        // the same close the while loop applies).
        self.close_loop_moves(base, moved_before, statement.range.start)?;
        statements.push(Statement::Declare(slot, start));
        statements.push(Statement::Declare(end_slot, end));
        statements.push(Statement::For(slot, end_slot, body, label, line, column));
        Ok(())
    }

    pub(super) fn lower_scalar_for_in(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        result: &mut Vec<Statement>,
    ) -> Result<(), ae_span::Diagnostic> {
        let ae_syntax::statement::StatementKind::ForIn { binding, binding_range, iterable, body } = &statement.kind else {
            unreachable!("resolved for-in eligibility");
        };
        self.seek_scalar_offset(binding_range.end);
        if RESERVED.contains(&binding.as_str()) {
            return Err(self.fail(&format!("'{binding}' is a built-in name and cannot be a loop variable")));
        }
        if self.names.contains_key(binding) {
            return Err(self.duplicate_binding_error(binding));
        }
        let (iterable_value, ty) = self.lower_scalar_body_expression(iterable, slots)?;
        self.seek_scalar_offset(iterable.range.end);
        let def = self.resolved_definition(
            self.resolved_expressions().scalar_definitions.get(&binding_range.start),
            binding_range.start, binding)?;
        let lower_body = |parser: &mut Self, slot, elem_ty| {
            slots.insert(def, (slot, elem_ty));
            let result = parser.lower_scalar_statements(body, slots);
            slots.remove(&def);
            result
        };
        if ty == Type::Tokens {
            self.lower_token_loop(binding, iterable_value, statement.range.start, result, lower_body)
        } else {
            self.lower_vector_loop(binding, iterable_value, ty, statement.range.start, result, lower_body)
        }
    }

    pub(super) fn lower_token_loop(
        &mut self, var_name: &str, first: Expr, loop_offset: usize,
        result: &mut Vec<Statement>,
        lower_body: impl FnOnce(&mut Self, usize, Type) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<(), ae_span::Diagnostic> {
        // `for token in cursor` streams the tokens as read-only
        // views: no vector, no per-token allocation, and the
        // loop body sees one token at a time (ADR-53). Each
        // loop restarts at the beginning of the base text.
        let cursor = match &first {
            Expr::Variable(id, Type::Tokens, _, _) => *id,
            _ => return Err(self.fail("for-in over a token cursor needs a bare cursor variable: bind it first, e.g. let toks = tokens(text, \" \");")),
        };
        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
        let (line, col) = self.position();
        let var = self.next_var; self.next_var += 1;
        let label = self.next_label; self.next_label += 1;
        self.loop_depth += 1;
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        self.scopes.push(vec![(var, Type::StrView)]);
        self.loop_scopes.push(self.scopes.len());
        self.loop_labels.push(Some(label));
        self.names.insert(var_name.to_owned(), (var, false, Type::StrView));
        let body = lower_body(self, var, Type::StrView);
        self.loop_labels.pop();
        self.loop_scopes.pop();
        self.scopes.pop().unwrap();
        self.loop_depth -= 1;
        self.names.remove(var_name);
        let body = body?;
        self.close_loop_moves(base, moved_before, loop_offset)?;
        result.push(Statement::ForTokens(cursor, var, body, label, line, col));
        Ok(())
    }

    pub(super) fn lower_vector_loop(
        &mut self, var_name: &str, first: Expr, first_ty: Type, loop_offset: usize,
        result: &mut Vec<Statement>,
        lower_body: impl FnOnce(&mut Self, usize, Type) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<(), ae_span::Diagnostic> {
        // `for x in vec-variable` iterates owned elements by
        // value: desugars to a hidden range loop plus an
        // immutable element declaration per iteration.
        // Borrowed string vectors iterate the same way, with
        // element access going through the pointer.
        let (v_id, elem_ty, by_ref, storage_elem, array_len, field_base) = match (&first, first_ty) {
            (Expr::Variable(id, Type::Vector(vid), _, _), Type::Vector(tid)) if *vid == tid => {
                let elem = self.ctx.vector_elem(tid);
                let loop_elem = self.loop_element_binding(elem);
                (*id, loop_elem, false, elem, None, None)
            }
            (Expr::Variable(id, Type::VectorRef(vid), _, _), Type::VectorRef(tid)) if *vid == tid => {
                let elem = self.ctx.vector_elem(tid);
                let loop_elem = self.loop_element_binding(elem);
                (*id, loop_elem, elem == Type::String, elem, None, None)
            }
            // `for x in slice` iterates a `&[T]` parameter (ADR-0236).
            (Expr::Variable(id, Type::Slice(vid), _, _), Type::Slice(tid)) if *vid == tid => {
                let elem = self.ctx.slice_elem(tid);
                let loop_elem = self.loop_element_binding(elem);
                (*id, loop_elem, false, elem, None, None)
            }
            // The same loop reads a mutable slice (`&mut [T]`, rung 4); the
            // loop variable is a copy/view, so iteration itself never writes.
            (Expr::Variable(id, Type::SliceMut(vid), _, _), Type::SliceMut(tid)) if *vid == tid => {
                let elem = self.ctx.slice_elem(tid);
                let loop_elem = self.loop_element_binding(elem);
                (*id, loop_elem, false, elem, None, None)
            }
            // A fixed array iterates too: the length is the type's constant,
            // and the element is a Copy value read by index.
            (Expr::Variable(id, Type::ArrayOf(aid, alen), _, _), Type::ArrayOf(tid, tlen))
                if *aid == tid && *alen == tlen =>
            {
                let elem = self.ctx.array_elem(tid);
                let loop_elem = self.loop_element_binding(elem);
                (*id, loop_elem, false, elem, Some(*alen), None)
            }
            // P2.4: a struct's vector field is a container (`for x in r.cells`).
            // The root binding is borrowed for the loop and every element reads
            // through the field's accessor. Owned element kinds keep the slot
            // form's in-place binding, which the field form does not carry yet.
            (Expr::Field(..), Type::Vector(tid)) => {
                let elem = self.ctx.vector_elem(tid);
                let root = field_root_slot(&first)
                    .ok_or_else(|| self.fail("for-in over a field needs a binding root"))?;
                if matches!(elem, Type::Vector(_))
                    || matches!(elem, Type::Struct(sid) if !self.structs[sid].is_copy)
                {
                    return Err(self.fail("for-in over a field of owned elements is not supported yet"));
                }
                let loop_elem = self.loop_element_binding(elem);
                (root, loop_elem, false, elem, None, Some(first.clone()))
            }
            (Expr::Field(..), other) => {
                return Err(self.fail(&format!("for-in over a field needs a vec, found {}", other.name(self.ctx))));
            }
            _ => return Err(self.fail("for-in needs a bare vec, slice or array variable or an int range (start..end)")),
        };
        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
        let (line, col) = self.position();
        let end_var = self.next_var; self.next_var += 1;
        let counter = self.next_var; self.next_var += 1;
        let var = self.next_var; self.next_var += 1;
        let label = self.next_label; self.next_label += 1;
        self.loop_depth += 1;
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        self.scopes.push(vec![(var, elem_ty), (counter, Type::Int(IntTy::I64)), (end_var, Type::Int(IntTy::I64))]);
        self.loop_scopes.push(self.scopes.len());
        self.loop_labels.push(Some(label));
        self.names.insert(var_name.to_owned(), (var, false, elem_ty));
        // The container is borrowed for the whole loop.
        let container_depth = self.scopes.len();
        self.borrows.push((v_id, container_depth, BorrowKind::Container));
        let body = lower_body(self, var, elem_ty);
        self.loop_labels.pop();
        self.loop_scopes.pop();
        self.scopes.pop().unwrap();
        self.loop_depth -= 1;
        self.names.remove(var_name);
        self.borrows.retain(|&(id, _, _)| id != v_id);
        let mut body = body?;
        if self.moved.contains(&v_id) { return Err(self.fail("cannot move the vector being iterated (a move of it happens in this loop)")); }
        self.close_loop_moves(base, moved_before, loop_offset)?;
        // Per-iteration element binding, then the user's body.
        let element = match elem_ty {
            // A fixed-array element is a Copy value read by the loop counter;
            // the length is the type's constant, not a runtime len().
            _ if array_len.is_some() => {
                let len = array_len.expect("an array loop carries its length");
                Expr::Index(v_id, len, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, storage_elem)
            }
            // A slice element reads through the fat pointer; the storage
            // element comes from the slice type, not the binding's view type.
            // An owned slice element binds through the same in-place address
            // the vector loop uses; string elements stay value-shaped views.
            _ if matches!(first_ty, Type::Slice(_) | Type::SliceMut(_)) => {
                let index = Expr::SliceIndex(v_id, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, storage_elem);
                if matches!(elem_ty, Type::VectorRef(_) | Type::StructRef(_)) {
                    Expr::BorrowAs(Box::new(index), false, line, col)
                } else {
                    index
                }
            }
            // A struct field container reads its element through the field's
            // accessor; the binding type already names the view.
            _ if field_base.is_some() => {
                let base = field_base.expect("a field loop carries its field expression");
                Expr::VecIndexOf(Box::new(base), Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, storage_elem)
            }
            Type::StrView => Expr::StrVecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, by_ref),
            // An owned element binds as a reference to the element's storage
            // (`&vec` for a nested vector): the loop variable never owns or
            // releases the inner value, and mutation through it is rejected
            // by the binding's type.
            Type::VectorRef(inner) => Expr::BorrowAs(
                Box::new(Expr::VecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, matches!(first_ty, Type::VectorRef(_)), Type::Vector(inner))),
                false, line, col,
            ),
            // An owned struct element binds the same way (`&Struct`).
            Type::StructRef(sid) => Expr::BorrowAs(
                Box::new(Expr::VecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, matches!(first_ty, Type::VectorRef(_)), Type::Struct(sid))),
                false, line, col,
            ),
            _ => Expr::VecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int(IntTy::I64), line, col)), line, col, matches!(first_ty, Type::VectorRef(_)), elem_ty),
        };
        body.insert(0, Statement::Declare(var, element));
        let end_init = match array_len {
            Some(len) => Expr::Number(len as i64),
            None => Expr::Len(Box::new(first), line, col),
        };
        result.push(Statement::Declare(end_var, end_init));
        result.push(Statement::Declare(counter, Expr::Number(0)));
        result.push(Statement::For(counter, end_var, body, label, line, col));
        Ok(())
    }

    pub(super) fn lower_scalar_loop_body(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        label: Option<usize>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        self.loop_depth += 1;
        self.loop_labels.push(label);
        let base = self.scopes.len();
        self.loop_scopes.push(base);
        let result = self.lower_scalar_statements(body, slots);
        self.loop_scopes.pop();
        self.loop_labels.pop();
        self.loop_depth -= 1;
        result
    }

    pub(super) fn lower_scalar_range_bound(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(expression, slots)?;
        self.seek_scalar_offset(expression.range.end);
        if ty != Type::Int(IntTy::I64) {
            return Err(self.fail("range bounds must be i64"));
        }
        Ok(value)
    }

    pub(super) fn lower_scalar_condition(
        &mut self,
        condition: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(condition, slots)?;
        self.seek_scalar_offset(condition.range.end);
        if ty != Type::Bool {
            return Err(self.fail("condition must be a boolean expression").with_code("E0205"));
        }
        Ok(value)
    }

    // A loop body runs again, so a value moved out of a variable declared
    // outside the loop must be re-initialized before the body ends; otherwise
    // the next iteration would read a moved-from handle. Without a fixed point
    // over the body (ADR-20) the move is rejected instead of guessed at.
    pub(super) fn close_loop_moves(&mut self, base: usize, before: std::collections::HashSet<usize>, offset: usize) -> Result<(), ae_span::Diagnostic> {
        let after = std::mem::replace(&mut self.moved, before.clone());
        let outer: Vec<usize> = self.scopes[..base.min(self.scopes.len())].iter().flatten().map(|&(id, _)| id).collect();
        for id in &after {
            // Values already moved before the loop are not this body's doing.
            if before.contains(id) || !outer.contains(id) { continue; }
            let name = self.names.iter().find(|(_, (vid, _, _))| vid == id).map(|(n, _)| n.clone()).unwrap_or_else(|| "?".to_owned());
            return Err(self.error_at(offset, &format!("cannot move '{name}' inside a loop body: the next iteration would read it after the move (re-initialize it before the body ends)")));
        }
        // Moves of loop-local values die with the body; nothing to merge.
        Ok(())
    }

    // break/continue leave the loop body: only owned values declared inside it die.
    pub(super) fn loop_local_owned(&self) -> Vec<(usize, Type)> {
        let base = *self.loop_scopes.last().unwrap_or(&0);
        self.scopes[base.min(self.scopes.len())..].iter().rev().flat_map(|s| s.iter().rev()).filter(|(_, ty)| self.is_owned(*ty)).map(|&(id, ty)| (id, ty)).collect()
    }

}

/// The type a `for` variable takes for an element: a string, an owned nested
/// vector and an owned struct bind as views (the container keeps the
/// storage), every other element binds as its value.
impl Parser<'_> {
    fn loop_element_binding(&self, elem: Type) -> Type {
        match elem {
            Type::String => Type::StrView,
            Type::Vector(inner) => Type::VectorRef(inner),
            Type::Struct(sid) if !self.structs[sid].is_copy => Type::StructRef(sid),
            other => other,
        }
    }
}

/// The binding slot a lowered field chain roots at (`r.cells` -> `r`).
fn field_root_slot(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Field(base, ..) => field_root_slot(base),
        Expr::Variable(id, ..) => Some(*id),
        _ => None,
    }
}
