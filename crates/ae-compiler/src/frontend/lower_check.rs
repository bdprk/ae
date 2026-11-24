//! Lowering module: check (P1.4 parser split).
use super::*;

impl<'a> Parser<'a> {
    pub(super) fn finish_exit(&mut self, code: Expr, ty: Type) -> Result<Statement, ae_span::Diagnostic> {
        if ty != Type::Int(IntTy::I64) {
            return Err(self.fail("exit expects an i64 status code"));
        }
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        Ok(Statement::Exit(code, self.live_owned()))
    }

    pub(super) fn check_view_binding(&self, mutable: bool, ty: Type) -> Result<(), ae_span::Diagnostic> {
        if mutable && matches!(ty, Type::StrView | Type::Tokens) {
            return Err(self.fail("a view binding cannot be mutable; declare it with 'let'"));
        }
        Ok(())
    }

    pub(super) fn check_borrowed_binding(&self, ty: Type) -> Result<(), ae_span::Diagnostic> {
        if matches!(ty, Type::VecRef | Type::StrVecRef) {
            return Err(self.fail("cannot store a borrowed vector in a variable; pass it directly to a parameter that borrows"));
        }
        if matches!(ty, Type::StructRef(_) | Type::StructMut(_)) {
            return Err(self.fail("cannot store a borrowed struct in a variable; pass it directly to a parameter that borrows"));
        }
        Ok(())
    }

    pub(super) fn pointer_store_pointee(&self, name: &str, ty: Option<Type>, line: usize) -> Result<Type, ae_span::Diagnostic> {
        let Some(Type::PtrMut(id)) = ty else {
            return Err(self.fail(&format!("'*{name}' requires a *mut pointer variable")));
        };
        if self.unsafe_depth == 0 {
            return Err(self.error_at(line, "writing through a raw pointer requires 'unsafe'").with_code("E0700"));
        }
        Ok(self.ctx.ptr_pointee(id))
    }

    pub(super) fn check_pointer_deref(&self, offset: usize) -> Result<(), ae_span::Diagnostic> {
        if self.unsafe_depth == 0 {
            return Err(self.error_at(offset, "dereferencing a raw pointer requires 'unsafe'").with_code("E0700"));
        }
        Ok(())
    }

    pub(super) fn check_pointer_store_value(&self, actual: Type, pointee: Type) -> Result<(), ae_span::Diagnostic> {
        if actual != pointee {
            return Err(self.fail(&format!("cannot write a {} value through *mut {}", actual.name(self.ctx), pointee.name(self.ctx))));
        }
        if pointee.owned() {
            return Err(self.fail("writing an owned value through a pointer is not supported yet; the pointer would take over its release"));
        }
        Ok(())
    }

    pub(super) fn checked_scalar(&self, range: Option<&std::ops::Range<usize>>) -> Option<ae_ty::Scalar> {
        match self.resolved_expressions().scalar_expression_types.get(range?)? {
            ae_resolve::resolve::ExpressionType::Checked(scalar) => Some(*scalar),
            _ => None,
        }
    }

    pub(super) fn check_compound_assignment(&self, target: Type, operator: &Kind) -> Result<(), ae_span::Diagnostic> {
        if (target == Type::String && *operator == Kind::Plus)
            || ae_ty::scalar_compound_assignment(target.scalar(), operator)
        {
            return Ok(());
        }
        Err(self.fail(&format!("compound assignment is not supported for type '{}'", target.name(self.ctx))))
    }

    pub(super) fn matches_value_type(&self, expected: Type, actual: Type, range: Option<&std::ops::Range<usize>>) -> bool {
        ae_ty::accepts_value(expected, actual, self.checked_scalar(range))
    }

    pub(super) fn check_return_type(&self, value: Type, range: Option<&std::ops::Range<usize>>) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(self.return_type, value, range) {
            return Ok(());
        }
        Err(self.fail(&format!("return type must be {}", self.return_type.name(self.ctx))).with_code("E0204"))
    }

    pub(super) fn check_value_argument_type(
        &mut self,
        name: &str,
        index: usize,
        expected: Type,
        actual: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(expected, actual, range) {
            return Ok(());
        }
        if let Some(range) = range {
            self.seek_scalar_offset(range.end);
        }
        Err(self.fail(&format!("argument {} to '{name}' must be {}", index + 1, expected.name(self.ctx))))
    }

    pub(super) fn check_field_value_type(
        &mut self,
        name: &str,
        expected: Type,
        actual: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(expected, actual, range) {
            return Ok(());
        }
        if let Some(range) = range {
            self.seek_scalar_offset(range.end);
        }
        Err(self.fail(&format!("field '{name}' must be {}", expected.name(self.ctx))))
    }

    pub(super) fn check_assignment_type(
        &self,
        name: &str,
        target: Type,
        value: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(target, value, range) {
            return Ok(());
        }
        Err(self.fail(&format!("cannot assign a {} value to {} variable '{name}'", value.name(self.ctx), target.name(self.ctx))))
    }

}
