//! Lowering module: shared (P1.4 parser split).
use super::*;
use super::parser::SharedExpressionEffects;

impl<'a> Parser<'a> {
    pub(super) fn lower_resolved_local(&mut self, name: &str, offset: usize) -> Option<(Expr, Type)> {
        let (id, _, ty) = *self.names.get(name)?;
        // An instantiated template parses in a sub-parser whose `subst` is
        // non-empty; that is not a reason to refuse the shared rules. The
        // binding-range identity check below is what proves the offset and
        // the slot name the same local, so the substitution is irrelevant.
        if self.moved.contains(&id) || self.shadow_ok {
            return None;
        }
        let definition = self.resolved_expressions().local_bindings.get(&offset)?;
        // Match the resolver's source definition to the active compiler slot.
        if self.binding_ranges.get(&id)? != definition {
            return None;
        }
        let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
        Some((Expr::Variable(id, ty, token.line, token.column), ty))
    }

    pub(super) fn lower_shared_argument(
        &mut self,
        argument: &ae_syntax::expression::Expression,
        expected: Type,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if (expected.owned() || expected == Type::StrView)
            && let ExpressionKind::Atom(Kind::Ident(name)) = &argument.kind {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(name, argument.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                if expected == ty && ty.owned() {
                    if self.variable_move_needs_deferral(slot)?
                        || effects.call_borrows.contains(&slot)
                        || effects.mut_borrows.contains(&slot)
                    {
                        effects.conflicts.push((slot, argument.range.start, name.clone(), "move", None));
                    }
                    effects.moves.push(slot);
                }
                return Some((value, ty));
            }
        if expected == Type::StrView {
            self.lower_shared_read(argument, remaining, nodes_left, effects)
        } else {
            self.lower_shared_expression(argument, remaining, nodes_left, effects)
        }
    }

    pub(super) fn lower_shared_field(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Field { base, name } = &expression.kind else {
            return None;
        };
        let remaining = remaining.checked_sub(1)?;
        let (value, ty) = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(binding)) => {
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(binding, base.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                (value, ty)
            }
            ExpressionKind::Field { .. } => self.lower_shared_field(base, remaining, nodes_left, effects)?,
            // The typed parser requires parentheses around a literal receiver.
            ExpressionKind::Struct { .. } => return None,
            _ => self.lower_shared_expression(base, remaining, nodes_left, effects)?,
        };
        let sid = match ty {
            Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
            _ => return None,
        };
        let (_, ty) = self.structs.get(sid)?.fields.iter().find(|(field, _)| field == name)?;
        Some((Expr::Field(Box::new(value), name.clone(), *ty), *ty))
    }

    pub(super) fn lower_shared_read(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            return matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::String | Type::StrView)
                .then_some((value, ty));
        }
        self.lower_shared_expression(expression, remaining, nodes_left, effects)
    }

    pub(super) fn lower_shared_variant(
        &mut self,
        target: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Qualified { base, name: variant } = &target.kind else {
            return None;
        };
        let member = self.tokens.partition_point(|token| token.offset < target.range.end).checked_sub(1)?;
        let offset = self.tokens[member].offset;
        if !self.resolved_expressions().enum_variants.contains(&offset) {
            return None;
        }
        let eid = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => *self.enum_names.get(name)?,
            ExpressionKind::Specialization { base, arguments } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return None };
                self.cached_enum_specialization(name, arguments)?
            }
            _ => return None,
        };
        let def = self.enums.get(eid)?;
        self.require_public("enum", def.name, &def.file, def.public).ok()?;
        let (variant_id, payload) = def.variant_payload(variant)?;
        let (enum_name, payload) = (def.name, payload.to_vec());
        let values = self.lower_shared_enum_payload(arguments, &payload, remaining, nodes_left, effects)?;
        Some((Expr::EnumLit(enum_name, eid, variant_id, values), Type::Enum(enum_name, eid)))
    }

    pub(super) fn lower_shared_enum_payload(
        &mut self,
        arguments: &[ae_syntax::expression::Expression],
        payload: &[Type],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<Vec<Expr>> {
        if arguments.len() != payload.len() {
            return None;
        }
        let mut values = Vec::with_capacity(payload.len());
        for (argument, &expected) in arguments.iter().zip(payload) {
            let (value, actual) = if expected == Type::String {
                self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?
            } else {
                self.lower_shared_expression(argument, remaining, nodes_left, effects)?
            };
            if !self.matches_value_type(expected, actual, Some(&argument.range)) {
                return None;
            }
            values.push(value);
        }
        Some(values)
    }

    pub(super) fn lower_shared_expression(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            // Owned fields need the typed parser's context-sensitive move checks.
            return matches!(ty, Type::Int | Type::F64 | Type::Bool).then_some((value, ty));
        }
        let remaining = remaining.checked_sub(1)?;
        // The typed Pratt parser charges operands, groups and prefixes.
        // Infix and postfix cast nodes add no charge beyond their operands.
        if !matches!(expression.kind, ExpressionKind::Binary { .. } | ExpressionKind::PointerCast { .. }) {
            *nodes_left = nodes_left.checked_sub(1)?;
        }
        match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => Some((Expr::Number(*value), Type::Int)),
            ExpressionKind::Atom(Kind::Float(value)) => Some((Expr::FloatLit(*value), Type::F64)),
            ExpressionKind::Atom(Kind::True) => Some((Expr::BoolLit(true), Type::Bool)),
            ExpressionKind::Atom(Kind::False) => Some((Expr::BoolLit(false), Type::Bool)),
            ExpressionKind::Atom(Kind::Text(value)) => {
                let offset = expression.range.start;
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
                Some((Expr::StrLit(value.clone(), token.line, token.column), Type::String))
            }
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let (_, _, ty) = self.names.get(name)?;
                if !matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::Closure(_) | Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                self.lower_resolved_local(name, expression.range.start)
            }
            ExpressionKind::Index { base, index } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return None;
                };
                let (Expr::Variable(slot, _, _, _), ty) = self.lower_resolved_local(name, base.range.start)? else {
                    return None;
                };
                if effects.moves.contains(&slot) {
                    return None;
                }
                let bracket = self.tokens.get(self.tokens.partition_point(|token| token.offset < base.range.end))?;
                if bracket.kind != Kind::LBracket {
                    return None;
                }
                let (line, column) = (bracket.line, bracket.column);
                let (index, Type::Int) = self.lower_shared_expression(index, remaining, nodes_left, effects)? else {
                    return None;
                };
                let index = Box::new(index);
                match ty {
                    Type::Array(len) => Some((Expr::Index(slot, len, index, line, column), Type::Int)),
                    Type::Vec | Type::VecRef | Type::VecF | Type::VecFRef => {
                        let borrowed = matches!(ty, Type::VecRef | Type::VecFRef);
                        let element = if matches!(ty, Type::VecF | Type::VecFRef) { Type::F64 } else { Type::Int };
                        Some((Expr::VecIndex(slot, index, line, column, borrowed, element), element))
                    }
                    _ => None,
                }
            }
            ExpressionKind::Struct { target, fields } => {
                if std::env::var_os("AE_DEBUG_MIR").is_some()
                    || !self.resolved_expressions().struct_constructors.contains(&target.range.start)
                {
                    return None;
                }
                let sid = match &target.kind {
                    ExpressionKind::Atom(Kind::Ident(name)) => *self.struct_names.get(name)?,
                    ExpressionKind::Specialization { base, arguments } => {
                        let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return None };
                        self.cached_struct_specialization(name, arguments)?
                    }
                    _ => return None,
                };
                self.lower_shared_struct_fields(sid, fields, remaining, nodes_left, effects)
            }
            ExpressionKind::Vector(elements) => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let (line, column) = (token.line, token.column);
                let mut vector_type = None;
                let mut values = Vec::new();
                for element in elements {
                    let (value, element_type) = self.lower_shared_expression(element, remaining, nodes_left, effects)?;
                    let ty = match element_type {
                        Type::Int => Type::Vec,
                        Type::F64 => Type::VecF,
                        _ => return None,
                    };
                    if vector_type.is_some_and(|previous| previous != ty) {
                        return None;
                    }
                    vector_type = Some(ty);
                    values.push(value);
                }
                let ty = vector_type.unwrap_or(Type::Vec);
                Some((Expr::VecLit(values, ty, line, column), ty))
            }
            ExpressionKind::AddressCast { binding, binding_range, mutable, pointee } => {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (operand, ty) = self.lower_resolved_local(binding, binding_range.start)?;
                let name = self.source.get(pointee.clone())?;
                if super::types::scalar_type(name)? != ty || (*mutable && !self.names.get(binding)?.1) {
                    return None;
                }
                let start = self.tokens.partition_point(|token| token.offset < expression.range.start);
                let token = self.tokens.get(start)?;
                let id = self.ctx.intern_pointee(ty);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::BorrowAs(Box::new(operand), *mutable, token.line, token.column), ty))
            }
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                let (operand, base_type) = self.lower_shared_expression(base, remaining, nodes_left, effects)?;
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                let name = self.source.get(pointee.clone())?;
                let pointee = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end)?;
                let id = self.ctx.intern_pointee(pointee);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty))
            }
            ExpressionKind::Group(inner) => self.lower_shared_expression(inner, remaining, nodes_left, effects),
            ExpressionKind::Qualified { .. } => {
                self.lower_shared_variant(expression, &[], remaining, nodes_left, effects)
            }
            ExpressionKind::Call { callee, arguments } => {
                if matches!(callee.kind, ExpressionKind::Qualified { .. }) {
                    if let Some(last) = arguments.last() {
                        let next = self.tokens.partition_point(|token| token.offset < last.range.end);
                        if self.tokens.get(next)?.kind != Kind::RParen {
                            return None;
                        }
                    }
                    return self.lower_shared_variant(callee, arguments, remaining, nodes_left, effects);
                }
                let (base, types) = match &callee.kind {
                    ExpressionKind::BracketedCallTarget { base, arguments } => (base.as_ref(), Some(arguments.as_slice())),
                    _ => (callee.as_ref(), None),
                };
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return None;
                };
                if self.names.contains_key(name) || RESERVED.contains(&name.as_str()) {
                    return None;
                }
                let resolved = self.resolved_expressions();
                let offset = base.range.start;
                let (id, params, ret) = if let Some(types) = types {
                    let (definition, (id, params, ret)) = self.cached_function_specialization(name, types)?;
                    let visible = if definition.file == self.file {
                        resolved.direct_calls.contains(&offset)
                    } else {
                        definition.file != PRELUDE && definition.public && resolved.unresolved_calls.contains(&offset)
                    };
                    if !visible {
                        return None;
                    }
                    (*id, params, *ret)
                } else {
                    let signature = self.functions.get(name)?;
                    if signature.file != self.file || signature.extern_c || !resolved.direct_calls.contains(&offset) {
                        return None;
                    }
                    (signature.id, &signature.params, signature.ret)
                };
                if self.ctx.is_unsafe_fn(id) || arguments.len() != params.len() || arguments.len() > 64
                    || !params.iter().all(|ty| {
                        matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView
                            | Type::Vec | Type::VecF | Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::Struct(_) | Type::Enum(..) | Type::StructRef(_) | Type::StructMut(_))
                    })
                    || !matches!(ret, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::VecF | Type::Struct(_) | Type::Enum(..))
                {
                    return None;
                }
                let params = params.clone();
                let args = self.lower_shared_call_arguments(arguments, &params, remaining, nodes_left, effects)?;
                Some((Expr::Call(id, args, ret), ret))
            }
            ExpressionKind::Prefix { operator, operand } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let position = (token.line, token.column);
                let operand = self.lower_shared_expression(operand, remaining, nodes_left, effects)?;
                self.lower_prefix(operator.clone(), operand, position, None).ok()
            }
            ExpressionKind::Binary { left, operator, right } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < left.range.end))?;
                if &token.kind != operator {
                    return None;
                }
                let position = (token.line, token.column);
                let lower_operand = if matches!(operator, Kind::Plus | Kind::EqEq | Kind::NotEq) {
                    Self::lower_shared_read
                } else {
                    Self::lower_shared_expression
                };
                let left = lower_operand(self, left, remaining, nodes_left, effects)?;
                let right = lower_operand(self, right, remaining, nodes_left, effects)?;
                self.lower_infix(left, operator.clone(), right, position, None).ok()
            }
            _ => None,
        }
    }

    pub(super) fn lower_shared_struct_fields(
        &mut self,
        sid: usize,
        fields: &[ae_syntax::expression::FieldInitializer],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        let def = self.structs.get(sid)?;
        self.require_public("struct", &def.name, &def.file, def.public).ok()?;
        if fields.len() != def.fields.len() {
            return None;
        }
        let declared_fields = def.fields.clone();
        let mut values: Vec<_> = (0..fields.len()).map(|_| None).collect();
        for field in fields {
            let slot = declared_fields.iter().position(|(name, _)| name == &field.name)?;
            if values[slot].is_some() {
                return None;
            }
            let expected = declared_fields[slot].1;
            let (value, actual) = self.lower_shared_argument(&field.value, expected, remaining, nodes_left, effects)?;
            if !self.matches_value_type(expected, actual, Some(&field.value.range)) {
                return None;
            }
            values[slot] = Some(value);
        }
        Some((Expr::StructLit(sid, values.into_iter().collect::<Option<_>>()?), Type::Struct(sid)))
    }

    pub(super) fn lower_committed_specialized_struct(&mut self, sid: usize) -> Option<(Expr, Type)> {
        if *self.current() != Kind::LBrace || std::env::var_os("AE_DEBUG_MIR").is_some() {
            return None;
        }
        let offset = self.tokens[self.pos].offset;
        let (end, fields) = self.resolved_expressions().specialized_struct_fields.get(&offset)?.clone();
        let cursor = self.tokens.partition_point(|token| token.offset < end);
        let close = self.tokens.get(cursor.checked_sub(1)?)?;
        if close.kind != Kind::RBrace || close.end != end {
            return None;
        }
        let depth = 128usize.checked_sub(self.expression_depth)?.min(64);
        let budget = 128usize.checked_sub(self.expression_nodes)?;
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let value = self.lower_shared_struct_fields(sid, &fields, depth, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.expression_nodes += budget - nodes_left;
        self.pos = cursor;
        Some(value)
    }

    pub(super) fn lower_shared_call_arguments(
        &mut self,
        arguments: &[ae_syntax::expression::Expression],
        params: &[Type],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<Vec<Argument>> {
        use ae_syntax::expression::ExpressionKind;
        let mut args = Vec::with_capacity(arguments.len());
        for (argument, expected) in arguments.iter().zip(params.iter().copied()) {
            if matches!(expected, Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_)) {
                let ExpressionKind::BorrowArgument { name, name_range, mutable } = &argument.kind else {
                    return None;
                };
                if *mutable != matches!(expected, Type::StructMut(_)) {
                    return None;
                }
                let (Expr::Variable(slot, ..), actual) = self.lower_resolved_local(name, name_range.start)? else {
                    return None;
                };
                if effects.moves.contains(&slot) {
                    return None;
                }
                use super::declarations::CallBorrow;
                let borrow = CallBorrow::classify(expected, actual)?;
                if let CallBorrow::Struct { .. } = borrow {
                    if !*mutable {
                        if effects.mut_borrows.contains(&slot)
                            || self.mut_borrows.iter().any(|&(borrowed, _)| borrowed == slot)
                        {
                            return None;
                        }
                        if !self.is_borrowed(slot) && !effects.call_borrows.contains(&slot) {
                            effects.call_borrows.push(slot);
                        }
                    } else {
                        if !self.names.get(name)?.1 || effects.call_borrows.contains(&slot)
                            || effects.mut_borrows.contains(&slot)
                        {
                            return None;
                        }
                        if self.is_borrowed(slot) {
                            if !self.only_view_borrows_active(slot) {
                                return None;
                            }
                            effects.conflicts.push((slot, name_range.start, name.clone(), "mutate", None));
                        }
                        effects.mut_borrows.push(slot);
                    }
                }
                args.push(Argument::Borrow(slot, borrow.address()));
                continue;
            }
            let value = self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?;
            args.push(match (expected, value) {
                (Type::StrView, (value, Type::String | Type::StrView)) => Argument::View(value),
                (expected, (value, actual)) if self.matches_value_type(expected, actual, Some(&argument.range)) => Argument::Value(value),
                _ => return None,
            });
        }
        Some(args)
    }

    pub(super) fn lower_committed_specialized_arguments(&mut self, params: &[Type]) -> Option<Vec<Argument>> {
        self.lower_indexed_argument_list(params, Self::lower_shared_call_arguments)
    }

    pub(super) fn lower_committed_enum_payload(&mut self, payload: &[Type]) -> Option<Vec<Expr>> {
        self.lower_indexed_argument_list(payload, Self::lower_shared_enum_payload)
    }

    pub(super) fn lower_indexed_argument_list<T>(
        &mut self,
        params: &[Type],
        lower: impl FnOnce(&mut Self, &[ae_syntax::expression::Expression], &[Type], usize, &mut usize, &mut SharedExpressionEffects) -> Option<T>,
    ) -> Option<T> {
        if *self.current() != Kind::LParen {
            return None;
        }
        let offset = self.tokens[self.pos].offset;
        let arguments = self.resolved_expressions().specialized_call_arguments.get(&offset)?.clone();
        if arguments.len() != params.len() || arguments.len() > 64 {
            return None;
        }
        let mut cursor = self.pos + 1;
        for (index, argument) in arguments.iter().enumerate() {
            if self.tokens.get(cursor)?.offset != argument.range.start {
                return None;
            }
            cursor = self.tokens.partition_point(|token| token.offset < argument.range.end);
            if index + 1 < arguments.len() {
                if self.tokens.get(cursor)?.kind != Kind::Comma {
                    return None;
                }
                cursor += 1;
            }
        }
        if self.tokens.get(cursor)?.kind != Kind::RParen {
            return None;
        }
        let depth = 128usize.checked_sub(self.expression_depth)?.min(64);
        let budget = 128usize.checked_sub(self.expression_nodes)?;
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let args = lower(self, &arguments, params, depth, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.expression_nodes += budget - nodes_left;
        self.pos = cursor + 1;
        Some(args)
    }

    pub(super) fn commit_shared_effects(&mut self, effects: SharedExpressionEffects) {
        self.call_borrows.extend(effects.call_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.mut_borrows.extend(effects.mut_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.deferred_write_conflicts.extend(effects.conflicts);
        self.moved.extend(effects.moves);
    }

    pub(super) fn shared_expression_start(kind: &Kind) -> bool {
        matches!(kind, Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False |
            Kind::Text(_) | Kind::LParen | Kind::Ident(_) | Kind::Amp)
            || ae_syntax::operator::prefix_binding_power(kind).is_some()
    }

    pub(super) fn lower_shared_value(&mut self, boundary: Option<Kind>, moving: bool) -> Option<(Expr, Type)> {
        if !Self::shared_expression_start(self.current()) {
            return None;
        }
        let offset = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&offset).cloned()?;
        let expression = match (boundary.as_ref(), &root.kind) {
            (Some(Kind::Question), ae_syntax::expression::ExpressionKind::Propagate(operand)) => operand.as_ref(),
            (Some(Kind::Question), _) => return None,
            _ => root.as_ref(),
        };
        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
        let next = &self.tokens.get(end)?.kind;
        // Leave the delimiter for the surrounding typed statement.
        // A propagation request lowers only the operand before '?'.
        let complete = match boundary.as_ref() {
            Some(expected) => next == expected,
            None => matches!(next, Kind::Semi | Kind::Comma | Kind::RParen | Kind::RBracket | Kind::DotDot),
        };
        if !complete {
            return None;
        }
        let (depth, budget) = if boundary.is_none() {
            (128usize.checked_sub(self.expression_depth)?.min(64),
             128usize.checked_sub(self.expression_nodes)?)
        } else {
            (64, 128)
        };
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let value = match &expression.kind {
            ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                if self.names.contains_key(name) =>
            {
                nodes_left = nodes_left.checked_sub(1)?;
                let value = self.lower_resolved_local(name, expression.range.start)?;
                let Expr::Variable(id, ty, ..) = &value.0 else { unreachable!("resolved local") };
                // Owned locals move on `moving`; copy-semantics handles
                // (Exec, Pool, Chan, Atomic, Mutex, Guard, TaskScope) and
                // views never move, so their reads route the same way the
                // typed parse reads them.
                if moving && ty.owned() {
                    let id = *id;
                    let start = self.pos;
                    self.pos = end;
                    if self.check_variable_move(id).is_err() {
                        self.pos = start;
                        return None;
                    }
                    self.moved.insert(id);
                }
                value
            }
            _ => self.lower_shared_expression(expression, depth, &mut nodes_left, &mut effects)?,
        };
        self.commit_shared_effects(effects);
        if boundary.is_none() {
            self.expression_nodes += budget - nodes_left;
        }
        self.pos = end;
        Some(value)
    }

}
