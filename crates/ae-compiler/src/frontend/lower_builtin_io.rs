//! Lowering module: builtin surface, the I/O, encoding, conversion,
//! JSON and math builtins (P1.4 parser split).
use super::*;

impl<'a> Parser<'a> {
    /// The I/O, encoding, conversion, JSON and math builtins. The name is
    /// known to be one of these when `lower_resolved_builtin` forwards it.
    pub(super) fn lower_resolved_builtin_surface(
        &mut self,
        name: &str,
        expression: &ae_syntax::expression::Expression,
        callee: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        
        (|| -> Result<(Expr, Type), ae_span::Diagnostic> {
            self.seek_scalar_offset(callee.range.start);
            let (line, column) = self.position();
            match name {
                "utf8_len" | "utf8_prefix" => {
                    let arity = if name == "utf8_len" { 1 } else { 2 };
                    self.expect_resolved_builtin_arity(expression, arguments, arity)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view")));
                    }
                    if name == "utf8_len" {
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::Utf8Len(Box::new(text), line, column), Type::Int(IntTy::I64)));
                    }
                    let (count, count_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if count_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("utf8_prefix expects an i64 character count"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Utf8Prefix(Box::new(text), Box::new(count), line, column), Type::String))
                }
                "count_lines_with" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("count_lines_with expects a string or string view"));
                    }
                    let (pattern, pattern_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(pattern_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("count_lines_with expects a string or string view pattern"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::CountLinesWith(Box::new(text), Box::new(pattern), line, column), Type::Int(IntTy::I64)))
                }
                "run_command" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("CommandResult");
                    let (program, program_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(program_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("run_command expects a string program name"));
                    }
                    let (arguments_value, arguments_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    let vector = match (&arguments_value, arguments_ty) {
                        (Expr::Variable(id, Type::Vector(vid), ..), Type::Vector(tid))
                            if *vid == tid && self.ctx.vector_elem(tid) == Type::String => *id,
                        _ => {
                            self.seek_scalar_offset(arguments[1].range.end);
                            return Err(self.fail("run_command expects a bare vec[string] variable for its arguments"));
                        }
                    };
                    if self.moved.contains(&vector) {
                        return Err(self.binding_label(self.fail("cannot use a moved vector in run_command").with_code("E0102"), vector));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::RunCommand(Box::new(program), vector, ty, line, column), ty))
                }
                "strvec_new" | "f64vec_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok(if name == "f64vec_new" {
                        let ty = Type::Vector(self.ctx.intern_vector_elem(Type::Float(FloatTy::F64)));
                        (Expr::F64VecNew(line, column), ty)
                    } else {
                        let ty = Type::Vector(self.ctx.intern_vector_elem(Type::String));
                        (Expr::StrVecNew(line, column), ty)
                    })
                }
                "lines" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("lines expects a string or string view"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let ty = Type::Vector(self.ctx.intern_vector_elem(Type::String));
                    Ok((Expr::Lines(Box::new(text), line, column), ty))
                }
                "join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    // join borrows a bare string-vector variable.
                    let (parts, parts_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let vid = match (&parts, parts_ty) {
                        (Expr::Variable(id, Type::Vector(vid), ..), Type::Vector(tid))
                            if *vid == tid && self.ctx.vector_elem(tid) == Type::String => *id,
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("join expects a bare vec[string] variable"));
                        }
                    };
                    if self.moved.contains(&vid) {
                        return Err(self.binding_label(self.fail("cannot use a moved vector in join").with_code("E0102"), vid));
                    }
                    let (sep, sep_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(sep_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("join expects a string or string view separator"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Join(vid, Box::new(sep), line, column), Type::String))
                }
                "slice_view" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (base, base_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    // The view borrows its base: a bare string variable, or a
                    // string field of a struct *variable* (the whole variable
                    // is what the borrow protects — ADR-52). Mirrors the typed
                    // route's checks and messages.
                    let (vid, field) = match (&base, base_ty) {
                        (Expr::Variable(id, Type::String, ..), Type::String) => (*id, None),
                        (Expr::Field(inner, fname, Type::String), Type::String) => match **inner {
                            Expr::Variable(id, Type::Struct(_), ..) => (id, Some(fname.clone())),
                            _ => {
                                self.seek_scalar_offset(arguments[0].range.end);
                                return Err(self.fail("slice_view can follow a string field only on a struct variable, not on a nested expression"));
                            }
                        },
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("slice_view expects a string variable or a string field of a struct variable (views borrow their base)"));
                        }
                    };
                    if self.moved.contains(&vid) {
                        return Err(self.binding_label(self.fail("cannot use a moved string in slice_view").with_code("E0102"), vid));
                    }
                    let (start, start_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if start_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("slice_view start must be i64"));
                    }
                    let (end, end_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                    if end_ty != Type::Int(IntTy::I64) {
                        self.seek_scalar_offset(arguments[2].range.end);
                        return Err(self.fail("slice_view end must be i64"));
                    }
                    let depth = self.scopes.len();
                    if !self.borrows.iter().any(|&(id, _, _)| id == vid) {
                        self.borrows.push((vid, depth, BorrowKind::View));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::SliceView(vid, field, Box::new(start), Box::new(end), line, column), Type::StrView))
                }
                "to_i64" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, actual) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if actual != Type::Float(FloatTy::F64) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("to_i64 expects an f64"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ToI64(Box::new(value), ty, line, column), ty))
                }
                "parse_int" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, actual) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(actual, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("parse_int expects a string or string view"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ParseInt(Box::new(value), ty, line, column), ty))
                }
                "find" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (doc, doc_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(doc_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("find expects a string to search in"));
                    }
                    let (needle, needle_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(needle_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("find expects a string to search for"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Find(Box::new(doc), Box::new(needle), ty, line, column), ty))
                }
                "json_get" | "json_int" | "json_bool" | "json_len" | "json_has" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (doc, doc_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(doc_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a JSON document string")));
                    }
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    let ty = match name {
                        "json_get" => self.prelude_type("ReadResult"),
                        "json_has" => Type::Bool,
                        "json_bool" => self.prelude_type("BoolResult"),
                        _ => self.prelude_type("IntResult"),
                    };
                    let value = match name {
                        "json_get" => Expr::JsonGet(Box::new(doc), Box::new(path), ty, line, column),
                        "json_int" => Expr::JsonInt(Box::new(doc), Box::new(path), ty, line, column),
                        "json_bool" => Expr::JsonBool(Box::new(doc), Box::new(path), ty, line, column),
                        "json_len" => Expr::JsonLen(Box::new(doc), Box::new(path), ty, line, column),
                        _ => Expr::JsonHas(Box::new(doc), Box::new(path), line, column),
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                _ => {
                    // A math builtin: fixed arity, every argument already f64.
                    let Some((index, _, arity)) = MATH_FNS.iter().enumerate().find_map(|(index, (math, _, arity))| (*math == name).then_some((index, *math, *arity))) else {
                        unreachable!("resolved builtin dispatch");
                    };
                    if arguments.len() < arity {
                        self.seek_scalar_offset(expression.range.end);
                        self.pos = self.pos.saturating_sub(1);
                        return Err(self.fail(&format!("{name} expects {arity} f64 argument(s)")));
                    }
                    if arguments.len() > arity {
                        self.seek_scalar_offset(arguments[arity - 1].range.end);
                        return Err(self.fail(&format!("{name} takes {arity} argument(s)")));
                    }
                    let mut values = Vec::with_capacity(arity);
                    for argument in arguments {
                        self.seek_scalar_offset(argument.range.start);
                        let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                        if ty != Type::Float(FloatTy::F64) {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail(&format!("{name} expects an f64 argument; convert integers explicitly with to_f64")));
                        }
                        values.push(Box::new(value));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MathCall(index as u8, values, line, column), Type::Float(FloatTy::F64)))
                }
            }
        })()
    }
}
