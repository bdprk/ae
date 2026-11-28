//! Declaration-site trait method checking for generic function bodies (P2.5.4).

use std::collections::HashMap;
use std::ops::Range;

use ae_syntax::declaration::TypedName;
use ae_syntax::expression::{Expression, ExpressionKind};
use ae_syntax::statement::{parse_block, ElseBranch, Statement, StatementKind};

use super::{Payload, Token, TraitDef, TraitMethod, Kind};

#[allow(clippy::too_many_arguments)]
pub(super) fn check_generic_fn_trait_methods(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    end: usize,
    fn_name: &str,
    params: &[String],
    trait_bounds: &[Vec<usize>],
    traits: &[TraitDef],
    parsed_parameters: Option<&[TypedName]>,
) -> Result<(), ae_span::Diagnostic> {
    if params.is_empty() || tokens.get(start).is_none_or(|t| t.kind != Kind::LParen) {
        return Ok(());
    }
    let Some(rparen) = matching_delim(tokens, start, end, Kind::LParen, Kind::RParen) else {
        return Ok(());
    };
    let Some(body_start) = ((rparen + 1)..end).find(|&i| tokens.get(i).is_some_and(|t| t.kind == Kind::LBrace)) else {
        return Ok(());
    };

    let Ok((stmts, _)) = parse_block(file, source, tokens, body_start) else {
        return Ok(());
    };

    let mut initial_scope: HashMap<String, Option<usize>> = HashMap::new();
    if let Some(parsed) = parsed_parameters {
        for param in parsed {
            let ty_str = source.get(param.ty.clone()).unwrap_or("");
            initial_scope.insert(param.name.clone(), type_param_index(ty_str, params));
        }
    } else {
        extract_params_from_tokens(source, tokens, start, rparen, params, &mut initial_scope);
    }

    let mut checker = Checker {
        file,
        source,
        tokens,
        fn_name,
        params,
        trait_bounds,
        traits,
        scopes: vec![initial_scope],
    };
    checker.check_statements(&stmts)
}

fn matching_delim(tokens: &[Token], start: usize, end: usize, open: Kind, close: Kind) -> Option<usize> {
    let mut depth = 0usize;
    for i in start..end {
        let kind = &tokens.get(i)?.kind;
        if *kind == open {
            depth += 1;
        } else if *kind == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn extract_params_from_tokens(
    source: &str,
    tokens: &[Token],
    lparen: usize,
    rparen: usize,
    params: &[String],
    out: &mut HashMap<String, Option<usize>>,
) {
    let mut piece = lparen + 1;
    let mut pieces = Vec::new();
    let mut depth = 0usize;
    for (i, token) in tokens.iter().enumerate().take(rparen).skip(lparen + 1) {
        match token.kind {
            Kind::LParen | Kind::LBracket => depth += 1,
            Kind::RParen | Kind::RBracket => depth = depth.saturating_sub(1),
            Kind::Comma if depth == 0 => {
                pieces.push((piece, i));
                piece = i + 1;
            }
            _ => {}
        }
    }
    pieces.push((piece, rparen));
    for (a, b) in pieces {
        let Some(colon) = (a..b).find(|&i| tokens[i].kind == Kind::Colon) else {
            continue;
        };
        if colon == a {
            continue;
        }
        let Kind::Ident(var) = &tokens[colon - 1].kind else {
            continue;
        };
        let ty_start = tokens.get(colon + 1).map_or(0, |t| t.offset);
        let ty_end = tokens.get(b - 1).map_or(ty_start, |t| t.end);
        let ty_str = source.get(ty_start..ty_end).unwrap_or("");
        out.insert(var.clone(), type_param_index(ty_str, params));
    }
}

fn type_param_index(ty_str: &str, params: &[String]) -> Option<usize> {
    let trimmed = ty_str.trim();
    let after_ref = trimmed.strip_prefix('&').map_or(trimmed, str::trim_start);
    let inner = after_ref
        .strip_prefix("mut")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        .map_or(after_ref, str::trim_start);
    params.iter().position(|p| p == inner)
}

struct Checker<'a> {
    file: &'a str,
    source: &'a str,
    tokens: &'a [Token],
    fn_name: &'a str,
    params: &'a [String],
    trait_bounds: &'a [Vec<usize>],
    traits: &'a [TraitDef],
    scopes: Vec<HashMap<String, Option<usize>>>,
}

impl<'a> Checker<'a> {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: String, ty: Option<usize>) {
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name, ty);
        }
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            if let Some(entry) = scope.get(name) {
                return *entry;
            }
        }
        None
    }

    fn check_block(&mut self, stmts: &[Statement]) -> Result<(), ae_span::Diagnostic> {
        self.push_scope();
        let res = self.check_statements(stmts);
        self.pop_scope();
        res
    }

    fn check_statements(&mut self, stmts: &[Statement]) -> Result<(), ae_span::Diagnostic> {
        for stmt in stmts {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_stmt(&mut self, stmt: &Statement) -> Result<(), ae_span::Diagnostic> {
        match &stmt.kind {
            StatementKind::Binding { name, ty, initializer, .. } => {
                self.check_expr(initializer)?;
                let bound_ty = match ty {
                    Some(range) => type_param_index(self.source.get(range.clone()).unwrap_or(""), self.params),
                    None => self.infer_expr_type_param(initializer),
                };
                self.bind(name.clone(), bound_ty);
            }
            StatementKind::Assign { target, value, .. } => {
                self.check_expr(target)?;
                self.check_expr(value)?;
            }
            StatementKind::PointerStore { value, .. }
            | StatementKind::Print(value)
            | StatementKind::Assert(value)
            | StatementKind::Return(value)
            | StatementKind::Expr { value, .. } => {
                self.check_expr(value)?;
            }
            StatementKind::If { condition, then_body, else_body } => {
                self.check_expr(condition)?;
                self.check_block(then_body)?;
                match else_body {
                    Some(ElseBranch::Block(block)) => self.check_block(block)?,
                    Some(ElseBranch::If(nested)) => self.check_stmt(nested)?,
                    None => {}
                }
            }
            StatementKind::While { condition, body } => {
                self.check_expr(condition)?;
                self.check_block(body)?;
            }
            StatementKind::ForRange { binding, start, end, body, .. } => {
                self.check_expr(start)?;
                self.check_expr(end)?;
                self.push_scope();
                self.bind(binding.clone(), None);
                let res = self.check_statements(body);
                self.pop_scope();
                res?;
            }
            StatementKind::ForIn { binding, iterable, body, .. } => {
                self.check_expr(iterable)?;
                self.push_scope();
                self.bind(binding.clone(), None);
                let res = self.check_statements(body);
                self.pop_scope();
                res?;
            }
            StatementKind::Match { subject, arms } => {
                self.check_expr(subject)?;
                for arm in arms {
                    self.check_block(&arm.body)?;
                }
            }
            StatementKind::Unsafe(body) => {
                self.check_block(body)?;
            }
            StatementKind::Break | StatementKind::Continue => {}
        }
        Ok(())
    }

    fn check_expr(&mut self, expr: &Expression) -> Result<(), ae_span::Diagnostic> {
        match &expr.kind {
            ExpressionKind::Call { callee, arguments } => {
                for arg in arguments {
                    self.check_expr(arg)?;
                }
                match &callee.kind {
                    ExpressionKind::Field { base, name: method_name } => {
                        self.check_expr(base)?;
                        if let Some(param_idx) = self.infer_expr_type_param(base) {
                            self.verify_trait_method_call(
                                param_idx,
                                method_name,
                                arguments.len(),
                                callee.range.clone(),
                                true,
                            )?;
                        }
                    }
                    ExpressionKind::Atom(Kind::Ident(method_name)) => {
                        if let Some(first) = arguments.first()
                            && let Some(param_idx) = self.infer_expr_type_param(first)
                            && self.has_trait_method_in_bounds(param_idx, method_name)
                        {
                            self.verify_trait_method_call(
                                param_idx,
                                method_name,
                                arguments.len(),
                                callee.range.clone(),
                                false,
                            )?;
                        }
                    }
                    _ => {
                        self.check_expr(callee)?;
                    }
                }
            }
            ExpressionKind::Group(inner)
            | ExpressionKind::Propagate(inner)
            | ExpressionKind::PointerCast { base: inner, .. }
            | ExpressionKind::ScalarCast { base: inner, .. }
            | ExpressionKind::Field { base: inner, .. }
            | ExpressionKind::Specialization { base: inner, .. }
            | ExpressionKind::BracketedCallTarget { base: inner, .. }
            | ExpressionKind::Qualified { base: inner, .. }
            | ExpressionKind::Prefix { operand: inner, .. } => {
                self.check_expr(inner)?;
            }
            ExpressionKind::Index { base, index } => {
                self.check_expr(base)?;
                self.check_expr(index)?;
            }
            ExpressionKind::Binary { left, right, .. } => {
                self.check_expr(left)?;
                self.check_expr(right)?;
            }
            ExpressionKind::Array(items) | ExpressionKind::Vector(items) => {
                for item in items {
                    self.check_expr(item)?;
                }
            }
            ExpressionKind::Struct { target, fields } => {
                self.check_expr(target)?;
                for field in fields {
                    self.check_expr(&field.value)?;
                }
            }
            ExpressionKind::Closure { params, body, .. } => {
                if let Ok(body_idx) = self.tokens.binary_search_by_key(&body.start, |t| t.offset)
                    && let Ok((stmts, _)) = parse_block(self.file, self.source, self.tokens, body_idx)
                {
                    self.push_scope();
                    for param in params {
                        let ty_str = self.source.get(param.ty.clone()).unwrap_or("");
                        self.bind(param.name.clone(), type_param_index(ty_str, self.params));
                    }
                    let res = self.check_statements(&stmts);
                    self.pop_scope();
                    res?;
                }
            }
            ExpressionKind::Atom(_)
            | ExpressionKind::Unit
            | ExpressionKind::AddressCast { .. }
            | ExpressionKind::BorrowArgument { .. } => {}
        }
        Ok(())
    }

    fn infer_expr_type_param(&self, expr: &Expression) -> Option<usize> {
        match &expr.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => self.lookup(name),
            ExpressionKind::Group(inner) => self.infer_expr_type_param(inner),
            ExpressionKind::BorrowArgument { name, fields, index, .. }
                if fields.is_empty() && index.is_none() =>
            {
                self.lookup(name)
            }
            ExpressionKind::Prefix { operator: Kind::Amp, operand } => {
                self.infer_expr_type_param(operand)
            }
            ExpressionKind::Call { callee, arguments } => {
                let (param_idx, method_name, is_method_syntax) = match &callee.kind {
                    ExpressionKind::Field { base, name } => {
                        (self.infer_expr_type_param(base)?, name.as_str(), true)
                    }
                    ExpressionKind::Atom(Kind::Ident(name)) => {
                        let first = arguments.first()?;
                        (self.infer_expr_type_param(first)?, name.as_str(), false)
                    }
                    _ => return None,
                };
                let method = self.find_unique_trait_method(param_idx, method_name)?;
                let expected = if is_method_syntax {
                    method.params.len().saturating_sub(1)
                } else {
                    method.params.len()
                };
                if arguments.len() == expected && matches!(method.returns, Payload::Param(0)) {
                    Some(param_idx)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn matching_trait_methods(&self, param_idx: usize, method_name: &str) -> Vec<(usize, &'a TraitMethod)> {
        let mut matches = Vec::new();
        for &tid in &self.trait_bounds[param_idx] {
            if let Some(trait_def) = self.traits.get(tid)
                && let Some(method) = trait_def.methods.iter().find(|m| m.name == method_name)
            {
                matches.push((tid, method));
            }
        }
        matches
    }

    fn has_trait_method_in_bounds(&self, param_idx: usize, method_name: &str) -> bool {
        !self.matching_trait_methods(param_idx, method_name).is_empty()
    }

    fn find_unique_trait_method(&self, param_idx: usize, method_name: &str) -> Option<&'a TraitMethod> {
        let matches = self.matching_trait_methods(param_idx, method_name);
        if matches.len() == 1 {
            Some(matches[0].1)
        } else {
            None
        }
    }

    fn verify_trait_method_call(
        &self,
        param_idx: usize,
        method_name: &str,
        actual_args: usize,
        range: Range<usize>,
        is_method_syntax: bool,
    ) -> Result<(), ae_span::Diagnostic> {
        let param_name = &self.params[param_idx];
        let bound_tids = &self.trait_bounds[param_idx];
        let matches = self.matching_trait_methods(param_idx, method_name);

        if matches.is_empty() {
            let msg = if bound_tids.is_empty() {
                format!(
                    "generic function '{}' calls method '{}' on unbounded type parameter '{}' (add a trait bound that defines '{}')",
                    self.fn_name, method_name, param_name, method_name
                )
            } else {
                let bounds_desc = bound_tids
                    .iter()
                    .filter_map(|&tid| self.traits.get(tid).map(|t| t.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(" + ");
                format!(
                    "generic function '{}' calls missing method '{}' on type parameter '{}' with bounds '{}' (no bound defines '{}')",
                    self.fn_name, method_name, param_name, bounds_desc, method_name
                )
            };
            return Err(ae_syntax::diagnostic_range(self.file, self.source, range, &msg));
        }

        if matches.len() > 1 {
            let trait_names = matches
                .iter()
                .map(|(tid, _)| self.traits[*tid].name.as_str())
                .collect::<Vec<_>>()
                .join("', '");
            let first = matches[0].1;
            let sig_conflict = matches.iter().skip(1).any(|(_, other)| {
                other.params != first.params || other.returns != first.returns
            });
            let msg = if sig_conflict {
                format!(
                    "generic function '{}' calls ambiguous method '{}' on type parameter '{}': bounds '{}' define conflicting signatures for '{}'",
                    self.fn_name, method_name, param_name, trait_names, method_name
                )
            } else {
                format!(
                    "generic function '{}' calls ambiguous method '{}' on type parameter '{}': multiple bounds ('{}') define '{}'",
                    self.fn_name, method_name, param_name, trait_names, method_name
                )
            };
            return Err(ae_syntax::diagnostic_range(self.file, self.source, range, &msg));
        }

        let (tid, method) = matches[0];
        let expected_args = if is_method_syntax {
            method.params.len().saturating_sub(1)
        } else {
            method.params.len()
        };
        if actual_args != expected_args {
            let msg = format!(
                "generic function '{}' calls trait method '{}::{}' on type parameter '{}' with {} argument(s), expected {}",
                self.fn_name, self.traits[tid].name, method_name, param_name, actual_args, expected_args
            );
            return Err(ae_syntax::diagnostic_range(self.file, self.source, range, &msg));
        }

        Ok(())
    }
}
