//! Lowering module: stmt (P1.4 parser split).
use super::*;
use super::parser::moved_self_assignment_hint;
use super::route_body_stmt::*;
use super::route_marks::*;

impl<'a> Parser<'a> {
    pub(super) fn lower_scalar_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::statement::{ElseBranch, StatementKind};
        let lowered = match &statement.kind {
            StatementKind::Binding { name, name_range, mutable, initializer } => {
                self.seek_scalar_offset(name_range.end);
                if RESERVED.contains(&name.as_str()) {
                    return Err(self.fail(&format!("'{name}' is a built-in name and cannot be a variable")));
                }
                if self.struct_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names a struct and cannot be a variable")));
                }
                if self.enum_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names an enum and cannot be a variable")));
                }
                if self.names.contains_key(name) {
                    return Err(self.duplicate_binding_error(name));
                }
                let slot = self.next_var;
                self.next_var += 1;
                let def = self.resolved_expressions().scalar_definitions[&name_range.start];
                let (statement, ty) = if let ae_syntax::expression::ExpressionKind::Propagate(operand) = &initializer.kind {
                    // `let x = foo()?;` binds the Ok payload and hands the
                    // failure on, mirroring the typed route's binding
                    // propagation.
                    let ae_syntax::expression::ExpressionKind::Call { callee, .. } = &operand.kind else {
                        return Err(self.fail("a propagation binding must call a function or construct a result variant"));
                    };
                    match &callee.kind {
                        ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))
                        | ae_syntax::expression::ExpressionKind::Qualified { .. } => {}
                        _ => return Err(self.fail("a propagation binding must call a function or construct a result variant")),
                    }
                    self.seek_scalar_offset(operand.range.start);
                    let (value, call_ty) = match self.lower_shared_value(Some(Kind::Question), true) {
                        Some(pair) => pair,
                        None => self.fallback_expression(0, true)?,
                    };
                    if *self.current() != Kind::Question {
                        return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
                    }
                    let (statement, bound_ty) = self.propagate_statement(value, call_ty, Some(slot))?;
                    self.pos += 1;
                    self.take(Kind::Semi)?;
                    let Some(bound_ty) = bound_ty else {
                        return Err(self.fail("'?' propagates a result enum, but this call does not return one"));
                    };
                    (statement, bound_ty)
                } else if let ae_syntax::expression::ExpressionKind::Array(elements) = &initializer.kind {
                    if elements.is_empty() {
                        self.seek_scalar_offset(initializer.range.start);
                        self.pos += 1;
                        return Err(self.fail("array must contain at least one element"));
                    }
                    let mut values = Vec::with_capacity(elements.len());
                    for (index, element) in elements.iter().enumerate() {
                        if index == 4096 {
                            self.seek_scalar_offset(element.range.start);
                            return Err(self.fail("array element limit exceeded (4096)"));
                        }
                        let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                        if ty != Type::Int {
                            self.seek_scalar_offset(element.range.end);
                            return Err(self.fail("array elements must be i64"));
                        }
                        values.push(value);
                    }
                    (Statement::Array(slot, values), Type::Array(elements.len()))
                } else if let ae_syntax::expression::ExpressionKind::Vector(elements) = &initializer.kind {
                    self.seek_scalar_offset(initializer.range.start);
                    let (line, column) = self.position();
                    let mut values = Vec::with_capacity(elements.len());
                    let mut element_type = None;
                    for (index, element) in elements.iter().enumerate() {
                        if index >= 65536 {
                            self.seek_scalar_offset(element.range.start);
                            return Err(self.fail("vector element limit exceeded (65536)"));
                        }
                        let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                        self.seek_scalar_offset(element.range.end);
                        element_type = Some(self.check_numeric_vector_element(element_type, ty)?);
                        values.push(value);
                    }
                    let ty = if element_type == Some(Type::F64) { Type::VecF } else { Type::Vec };
                    (Statement::Declare(slot, Expr::VecLit(values, ty, line, column)), ty)
                } else {
                    let (value, ty) = if let ae_syntax::expression::ExpressionKind::Closure { .. } = &initializer.kind {
                        // A closure literal: the typed closure machinery owns
                        // capture inference, the generated function, and the
                        // body route (it lowers the body through this same
                        // resolved pass).
                        self.seek_scalar_offset(initializer.range.start);
                        let literal = self.closure_literal()?;
                        self.seek_scalar_offset(initializer.range.end);
                        literal
                    } else if iterator_constructor(initializer).is_some() {
                        self.seek_scalar_offset(initializer.range.start);
                        self.fallback_expression(0, false)?
                    } else {
                        let lowered = self.lower_scalar_body_expression(initializer, slots)?;
                        self.seek_scalar_offset(initializer.range.end);
                        lowered
                    };
                    // An owned field cannot be moved out of its struct: the
                    // typed route's rule (a struct owns its fields as a whole).
                    if ty.owned()
                        && matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Field { .. }) {
                            self.seek_scalar_offset(initializer.range.end);
                            return Err(self.fail("cannot move a field out of a struct; use it where a borrow works"));
                        }
                    // Binding an owned value moves it: the new slot owns what
                    // the old one did, and later reads of the source fail with
                    // E0102 (the typed route's rule).
                    if ty.owned()
                        && let Expr::Variable(source, ..) = &value {
                            self.check_variable_move(*source)?;
                            self.moved.insert(*source);
                        }
                    // Views borrow their base: `let w = v[0];` registers the vector so a
                    // later `pop(v)`/`push(v, ..)` is rejected while `w` can
                    // still be read (the typed block path's rule).
                    self.record_borrow_bases(&value);
                    self.check_view_binding(*mutable, ty)?;
                    self.take(Kind::Semi)?;
                    self.check_borrowed_binding(ty)?;
                    (Statement::Declare(slot, value), ty)
                };
                slots.insert(def, (slot, ty));
                self.names.insert(name.clone(), (slot, *mutable, ty));
                self.binding_ranges.insert(slot, name_range.clone());
                if ty.owned() {
                    self.scopes.last_mut().expect("block scope").push((slot, ty));
                }
                statement
            }
            StatementKind::Assign { target, operator, value } => {
                if let ae_syntax::expression::ExpressionKind::Field { base, name } = &target.kind {
                    let def = self.resolved_expressions().scalar_writes[&base.range.start];
                    // The store writes through a `&mut` receiver or into an
                    // owned local struct; both reach the same FieldStore, and
                    // codegen picks `.`/`->` from the receiver type.
                    let (slot, receiver) = slots[&def];
                    if let Type::StructRef(_) = receiver {
                        // A read-only struct borrow: neither the pointer nor
                        // anything behind it may be written. The typed route
                        // distinguishes an immutable binding from a mutable one
                        // (`var r: &Rect`), and so does this arm.
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind else {
                            unreachable!("borrowed struct store base");
                        };
                        if !self.names.get(base_name).is_some_and(|&(_, mutable, _)| mutable) {
                            self.seek_scalar_offset(target.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{base_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        self.seek_scalar_offset(base.range.end);
                        return Err(self.fail(&format!("cannot write through '{base_name}': struct borrows are read-only in this version")));
                    }
                    let sid = match receiver {
                        Type::StructMut(sid) | Type::Struct(sid) | Type::StructRef(sid) => sid,
                        other => return Err(self.error_at(target.range.start, &format!("type {} has no fields or methods", other.name(self.ctx)))),
                    };
                    if matches!(receiver, Type::Struct(_)) {
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind else {
                            unreachable!("owned struct store base");
                        };
                        if !self.names[base_name].1 {
                            self.seek_scalar_offset(target.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{base_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                    }
                    let field = field_ty_in(sid, name, self.structs)
                        .ok_or_else(|| self.error_at(target.range.start, &format!("struct '{}' has no field '{name}'", self.structs[sid].name)))?;
                    let compound = compound_assignment_operator(operator);
                    // The check takes the operator's plain form (`+=` -> `+`),
                    // exactly like the typed store.
                    if let Some(op) = &compound {
                        self.check_compound_assignment(field, op)?;
                    }
                    self.seek_scalar_offset(value.range.start);
                    let (line, column) = self.position();
                    let (value, actual) = self.lower_scalar_body_expression(value, slots)?;
                    // An owned value moves into the field; the source slot is
                    // forfeited, exactly like a struct literal field.
                    if actual.owned()
                        && let Expr::Variable(source, ..) = &value {
                            self.check_variable_move(*source)?;
                            self.moved.insert(*source);
                        }
                    self.seek_scalar_offset(statement.range.end);
                    self.check_field_value_type(name, field, actual, None)?;
                    // `c.n += 5` stores `c.n + 5`: the typed route's FieldStore
                    // with the field read applied in place.
                    let stored = match compound {
                        Some(op) => {
                            let current = Expr::Field(Box::new(Expr::Variable(slot, receiver, line, column)), name.clone(), field);
                            Expr::Binary(Box::new(current), op, Box::new(value), line, column, field)
                        }
                        None => value,
                    };
                    return Ok(Statement::FieldStore(slot, name.clone(), field, stored, receiver));
                }
                if let ae_syntax::expression::ExpressionKind::Index { base, index } = &target.kind {
                    // An array element store (`a[i] = v`): eligibility
                    // confirmed a resolved array binding and the plain
                    // operator. The checks mirror the typed store: the
                    // binding must be mutable (E0103), the index and the
                    // element must be i64 (E0201 for the index). A container
                    // held in a struct field (`p.table[k] = v`) resolves the
                    // field's declared type and keeps the field as the place.
                    let (slot, ty, root_name, field_place) = if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &base.kind {
                        let def = self.resolved_expressions().scalar_writes.get(&base.range.start)
                            .or_else(|| self.resolved_expressions().scalar_reads.get(&base.range.start))
                            .copied()
                            .ok_or_else(|| self.error_at(base.range.start, &format!("unknown variable '{name}'")).with_code("E0101"))?;
                        let (slot, ty) = slots[&def];
                        (slot, ty, name.clone(), None)
                    } else if let ae_syntax::expression::ExpressionKind::Field { base: root, name: field } = &base.kind {
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(root_name)) = &root.kind else {
                            return Err(self.error_at(base.range.start, "an index store needs a named container"));
                        };
                        let &(slot, _, struct_ty) = self.names.get(root_name)
                            .ok_or_else(|| self.error_at(root.range.start, &format!("unknown variable '{root_name}'")).with_code("E0101"))?;
                        let sid = match struct_ty {
                            Type::Struct(sid) | Type::StructMut(sid) => sid,
                            _ => return Err(self.error_at(root.range.start, "only a named struct field holds a container here")),
                        };
                        let field_ty = field_ty_in(sid, field, self.structs)
                            .ok_or_else(|| self.error_at(base.range.start, &format!("struct '{}' has no field '{field}'", self.structs[sid].name)))?;
                        (slot, field_ty, root_name.clone(), Some(field.clone()))
                    } else {
                        return Err(self.error_at(base.range.start, "an index store needs a named container"));
                    };
                    // `m[key] = v` inserts or overwrites in a map: the key is
                    // copied by the runtime, the value must match the map's
                    // element type, and the store is statement-scoped.
                    if matches!(ty, Type::Map | Type::MapF) {
                        let kind = map_kind_of(ty);
                        if !self.names[&root_name].1 {
                            self.seek_scalar_offset(base.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{root_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        if self.is_borrowed(slot) {
                            self.seek_scalar_offset(base.range.end);
                            return Err(self.fail(&format!("cannot modify '{root_name}' while it is borrowed")));
                        }
                        self.seek_scalar_offset(base.range.end);
                        let (line, column) = self.position();
                        let (key, key_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if !matches!(key_ty, Type::String | Type::StrView) {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("map keys must be strings"));
                        }
                        let (stored, value_ty) = self.lower_scalar_body_expression(value, slots)?;
                        if value_ty != map_element_ty(kind) {
                            self.seek_scalar_offset(value.range.end);
                            return Err(self.fail(&format!("map values must be {}", map_element_ty(kind).name(self.ctx))));
                        }
                        self.seek_scalar_offset(value.range.end);
                        self.take(Kind::Semi)?;
                        let place = match field_place {
                            Some(field) => MapPlace::Field(slot, field, kind),
                            None => MapPlace::Var(slot, kind),
                        };
                        return Ok(Statement::MapPut(place, key, stored, line, column));
                    }
                    if matches!(ty, Type::Vec | Type::StrVec | Type::VecF) {
                        if field_place.is_some() {
                            return Err(self.error_at(base.range.start, "an element store into a vector field is not supported by the body route yet"));
                        }
                        // A vector element store: overwriting a string element
                        // releases the old one, so a live borrow blocks it
                        // (the typed store's rule).
                        if !self.names[&root_name].1 {
                            self.seek_scalar_offset(base.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{root_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        if self.is_borrowed(slot) {
                            self.seek_scalar_offset(base.range.end);
                            return Err(self.fail(&format!("cannot modify '{root_name}' while it is borrowed")));
                        }
                        self.seek_scalar_offset(base.range.end);
                        let (line, column) = self.position();
                        let (element_index, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let (stored, value_ty) = self.lower_scalar_body_expression(value, slots)?;
                        let expected = match ty {
                            Type::StrVec => "string",
                            Type::VecF => "f64",
                            _ => "i64",
                        };
                        let matches_element = match ty {
                            Type::StrVec => matches!(value_ty, Type::String | Type::StrView),
                            Type::VecF => value_ty == Type::F64,
                            _ => value_ty == Type::Int,
                        };
                        if !matches_element {
                            self.seek_scalar_offset(value.range.end);
                            return Err(self.fail(&format!("vector elements must be {expected}")));
                        }
                        // A value store moves an owned string into the slot;
                        // a view store only borrows it.
                        if value_ty.owned()
                            && let Expr::Variable(source, ..) = &stored {
                                self.moved.insert(*source);
                            }
                        self.seek_scalar_offset(value.range.end);
                        self.take(Kind::Semi)?;
                        return Ok(match ty {
                            Type::StrVec => Statement::StrVecStore(slot, element_index, stored, line, column, value_ty == Type::StrView),
                            Type::VecF => Statement::VecFStore(slot, element_index, stored, line, column),
                            _ => Statement::VecStore(slot, element_index, stored, line, column),
                        });
                    }
                    let Type::Array(len) = ty else {
                        unreachable!("resolved array store");
                    };
                    if !self.names[&root_name].1 {
                        self.seek_scalar_offset(base.range.start);
                        let diagnostic = self.fail(&format!("cannot assign to immutable variable '{root_name}'")).with_code("E0103");
                        return Err(self.immutable_binding_error(diagnostic, slot));
                    }
                    self.seek_scalar_offset(base.range.end);
                    let (line, column) = self.position();
                    let element_index = self.lower_scalar_array_index(index, slots)?;
                    self.seek_scalar_offset(value.range.start);
                    let (element, element_ty) = self.lower_scalar_body_expression(value, slots)?;
                    if element_ty != Type::Int {
                        self.seek_scalar_offset(value.range.end);
                        return Err(self.fail("array elements must be i64"));
                    }
                    self.seek_scalar_offset(statement.range.end);
                    return Ok(Statement::Store(slot, len, element_index, element, line, column));
                }
                let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &target.kind else {
                    unreachable!("scalar assignment eligibility");
                };
                let def = self.resolved_expressions().scalar_writes[&target.range.start];
                let (slot, ty) = slots[&def];
                if !self.names[name].1 {
                    self.seek_scalar_offset(target.range.start);
                    let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                    return Err(self.immutable_binding_error(diagnostic, slot));
                }
                // A `&mut` borrow is a view of the caller's storage: writing
                // the binding itself would re-point the view. Fields are the
                // writable surface, exactly as the typed route says.
                if matches!(ty, Type::StructMut(_)) {
                    // The typed route points at the `=` after the name.
                    self.seek_scalar_offset(target.range.end);
                    return Err(self.fail(&format!("cannot assign to '{name}' itself through a '&mut' borrow; write its fields instead")));
                }
                // Writing an owned value while a view of it may still be read
                // frees the bytes the view points at. A string's view
                // conflicts defer to the graph's ordered adjudication (the
                // typed rule); every other owned type rejects on sight.
                if ty.owned() && self.is_borrowed(slot) {
                    if !(matches!(ty, Type::String) && self.all_borrows_are_views(slot)) {
                        self.seek_scalar_offset(target.range.end);
                        return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                    }
                    self.deferred_write_conflicts.push((slot, target.range.start, name.clone(), "assign to", None));
                }
                self.seek_scalar_offset(target.range.end);
                let compound = compound_assignment_operator(operator);
                if let Some(op) = &compound {
                    self.check_compound_assignment(ty, op)?;
                }
                self.seek_scalar_offset(value.range.start);
                let (line, column) = self.position();
                // The right-hand side may not consume the target itself.
                // Assigning to an already-moved variable is a legal
                // re-initialization, so the flag clears either way.
                let moved_before = self.moved.contains(&slot);
                let (rhs, rhs_ty) = self.lower_scalar_body_expression(value, slots)?;
                // An owned right-hand side moves into the target: the source
                // slot's Frees is trimmed, and assigning the target to itself
                // fails as a same-statement move (the typed rule).
                if rhs_ty.owned()
                    && let Expr::Variable(source, ..) = &rhs {
                        self.seek_scalar_offset(value.range.end);
                        self.check_variable_move(*source)?;
                        self.moved.insert(*source);
                    }
                self.seek_scalar_offset(statement.range.end);
                self.check_assignment_type(name, ty, rhs_ty, Some(&value.range))?;
                if self.moved.contains(&slot) && !moved_before {
                    return Err(moved_self_assignment_hint(self.binding_label(
                        self.error_at(target.range.start, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), slot)));
                }
                self.moved.remove(&slot);
                let value = if let Some(op) = compound {
                    let lhs = Expr::Variable(slot, ty, line, column);
                    Expr::Binary(Box::new(lhs), op, Box::new(rhs), line, column, ty)
                } else {
                    rhs
                };
                // Overwriting an owned value releases the old one first (the
                // Assign codegen's flag); scalars need no release.
                Statement::Assign(slot, value, ty.owned())
            }
            StatementKind::PointerStore { pointer, pointer_range, value } => {
                self.seek_scalar_offset(statement.range.start);
                let (line, column) = self.position();
                let def = self.resolved_expressions().scalar_writes[&pointer_range.start];
                let (slot, ty) = slots[&def];
                self.seek_scalar_offset(pointer_range.end);
                let pointee = self.pointer_store_pointee(pointer, Some(ty), line)?;
                let (lowered, actual) = self.lower_scalar_body_expression(value, slots)?;
                self.seek_scalar_offset(value.range.end);
                self.check_pointer_store_value(actual, pointee)?;
                self.take(Kind::Semi)?;
                Statement::PtrStore(slot, lowered, line, column)
            }
            StatementKind::Print(argument) => {
                // A literal prints as `Printed::Text`, the same statement
                // the typed route emits for it (byte parity).
                if let ae_syntax::expression::ExpressionKind::Atom(Kind::Text(text)) = &argument.kind {
                    self.seek_scalar_offset(statement.range.end);
                    return Ok(Statement::Print(Printed::Text(text.clone())));
                }
                let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail(&format!("cannot print a value of type {}", ty.name(self.ctx))).with_code("E0206"));
                }
                Statement::Print(Printed::Expr(value))
            }
            StatementKind::Assert(argument) => {
                self.seek_scalar_offset(statement.range.start);
                let (line, column) = self.position();
                let (condition, ty) = self.lower_scalar_body_expression(argument, slots)?;
                if ty != Type::Bool {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail("assert requires a boolean condition"));
                }
                Statement::Assert(condition, line, column)
            }
            StatementKind::Return(value) => {
                if self.in_main {
                    self.seek_scalar_offset(statement.range.start);
                    return Err(self.fail("return is not supported in main"));
                }
                if matches!(&value.kind, ae_syntax::expression::ExpressionKind::Closure { .. }) {
                    self.seek_scalar_offset(statement.range.end);
                    return Err(self.fail("a closure cannot be returned: it has no storage, and its captures live in this frame"));
                }
                let (lowered, ty) = self.lower_scalar_body_expression(value, slots)?;
                // Returning an owned value moves it out of the body: the
                // caller owns it, so the drops pass must trim this slot's
                // Frees and later reads are rejected (E0102). This covers
                // `return self;` and `return binding;` for structs, strings
                // and vectors alike.
                // An owned field cannot be moved out of its struct (the
                // struct owns its fields as a whole), so `return p.text;` is
                // the typed route's rejection, not a value.
                if ty.owned()
                    && matches!(&value.kind, ae_syntax::expression::ExpressionKind::Field { .. }) {
                        self.seek_scalar_offset(value.range.end);
                        return Err(self.fail("cannot move a field out of a struct; use it where a borrow works"));
                    }
                if let Expr::Variable(slot, ty, ..) = &lowered
                    && ty.owned() {
                        self.seek_scalar_offset(value.range.end);
                        self.check_variable_move(*slot)?;
                        self.moved.insert(*slot);
                    }
                self.seek_scalar_offset(statement.range.end);
                self.check_return_type(ty, Some(&value.range))?;
                Statement::Return(lowered, self.live_owned())
            }
            StatementKind::If { condition, then_body, else_body } => {
                let value = self.lower_scalar_condition(condition, slots)?;
                let before = self.moved.clone();
                let yes = self.lower_scalar_statements(then_body, slots)?;
                let after_yes = std::mem::replace(&mut self.moved, before);
                let no = if let Some(branch) = else_body {
                    self.pos += 1;
                    match branch {
                        ElseBranch::Block(body) => self.lower_scalar_statements(body, slots)?,
                        ElseBranch::If(statement) => vec![self.lower_scalar_statement(statement, slots)?],
                    }
                } else {
                    Vec::new()
                };
                self.moved.extend(after_yes);
                Statement::If(value, yes, no)
            }
            StatementKind::While { condition, body } => {
                let condition = self.lower_scalar_condition(condition, slots)?;
                if self.loop_depth >= 64 {
                    return Err(self.fail("loop nesting limit exceeded (64)"));
                }
                let base = self.scopes.len();
                let moved_before = self.moved.clone();
                let loop_body = self.lower_scalar_loop_body(body, slots, None)?;
                self.close_loop_moves(base, moved_before, statement.range.start)?;
                Statement::While(condition, loop_body)
            }
            StatementKind::Unsafe(body) => {
                // An `unsafe { ... }` block is a scoping container; mirror
                // the typed route's depth tracking and the W0702 note for
                // blocks with no unchecked operation.
                self.seek_scalar_offset(statement.range.start);
                self.pos += 1;
                let block_line = self.tokens[self.pos].line;
                let block_column = self.tokens[self.pos].column;
                self.unsafe_depth += 1;
                let inner = self.lower_scalar_statements(body, slots);
                self.unsafe_depth -= 1;
                let inner = inner?;
                if !self.block_has_unchecked_operation(&inner) {
                    eprintln!("ae: {}:{}: warning: unnecessary unsafe block (W0702): the body contains no unchecked operation", block_line, block_column);
                }
                Statement::Unsafe(inner)
            }
            StatementKind::Expr { .. } if is_task_yield(statement) => {
                let StatementKind::Expr { value, .. } = &statement.kind else { unreachable!("task_yield statement") };
                self.seek_scalar_offset(value.range.end);
                self.take(Kind::Semi)?;
                return Ok(Statement::TaskYield);
            }
            StatementKind::Expr { propagates: false, .. }
                if vector_statement_builtin(statement, self.resolved_expressions()).is_some() =>
            {
                return self.lower_vector_builtin_statement(statement, slots);
            }
            StatementKind::Expr { propagates: false, .. } if handle_statement_builtin(statement).is_some() => {
                return self.lower_handle_builtin_statement(statement, slots);
            }
            StatementKind::Expr { propagates: true, .. } => {
                let operand = propagation_call(statement).expect("resolved propagation eligibility");
                self.seek_scalar_offset(operand.range.start);
                self.lower_propagating_call()?
            }
            StatementKind::Expr { .. } if bare_call_statement(statement).is_some() => {
                let StatementKind::Expr { value, .. } = &statement.kind else { unreachable!("unknown call statement") };
                self.seek_scalar_offset(value.range.start);
                // The name answers nowhere, so the callee resolution fails
                // with the typed route's E0207 (the error precedes the
                // statement-level "must end with '?'" check). If it somehow
                // resolves, fall through to that statement rule.
                let _ = self.lower_scalar_body_expression(value, slots)?;
                return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
            }
            StatementKind::Expr { .. } => {
                // Not `exit` and not a bare call the earlier arm took (a bare
                // method call, say): the typed route's statement rule.
                let Some(status) = exit_status_operand(statement) else {
                    return Err(self.error_at(statement.range.start, "a statement that only calls a function must end with '?' to propagate its result"));
                };
                let (code, ty) = self.lower_scalar_body_expression(status, slots)?;
                self.seek_scalar_offset(status.range.end);
                self.finish_exit(code, ty)?
            }
            StatementKind::Match { subject, arms } => {
                let (value, ty) = self.lower_scalar_body_expression(subject, slots)?;
                self.seek_scalar_offset(subject.range.end);
                self.lower_match(value, ty, |parser, index, names| {
                    let definitions = names.iter().map(|(_, offset)| {
                        parser.resolved_expressions().scalar_definitions.get(offset).copied()
                    }).collect::<Option<Vec<_>>>();
                    let (Some(arm), Some(definitions)) = (arms.get(index), definitions) else {
                        // The arm's payload bindings have no resolved identity:
                        // a permissive match subject that the subject check
                        // will reject; report the shape instead of parsing the
                        // body with the removed typed parser.
                        return Err(parser.fail("this match arm's payload bindings are not on the resolved route"));
                    };
                    for ((name, _), &def) in names.iter().zip(&definitions) {
                        let (slot, _, ty) = parser.names[name];
                        slots.insert(def, (slot, ty));
                    }
                    let body = parser.lower_scalar_statements(&arm.body, slots);
                    for def in definitions { slots.remove(&def); }
                    body
                })?
            }
            StatementKind::Break | StatementKind::Continue => {
                let is_break = matches!(statement.kind, StatementKind::Break);
                if self.loop_depth == 0 {
                    self.seek_scalar_offset(statement.range.start);
                    let keyword = if is_break { "break" } else { "continue" };
                    return Err(self.fail(&format!("'{keyword}' outside of a loop")));
                }
                if is_break {
                    Statement::Break
                } else {
                    Statement::Continue(self.loop_labels.last().copied().flatten())
                }
            }
            _ => return Err(self.error_at(statement.range.start, "this statement is not supported by the body route")),
        };
        self.seek_scalar_offset(statement.range.end);
        Ok(lowered)
    }

    pub(super) fn field_ty_checked(&self, sid: usize, name: &str) -> Result<Type, ae_span::Diagnostic> {
        let defn = &self.structs[sid];
        defn.fields.iter().position(|(n, _)| n == name)
            .map(|fidx| defn.fields[fidx].1)
            .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", defn.name, name)))
    }

}
