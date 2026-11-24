//! Statement grammar shared by the standalone parser and, eventually, the
//! compiler frontend (ADR-0224 R3a).
//!
//! The tree here is grammar-only: names, types, and storage stay unresolved.
//! Anything the compiler rejects for semantic reasons (immutable assignment,
//! moved values, non-exhaustive match) is accepted here on purpose, so the
//! grammar stays a grammar and the resolver keeps its diagnostics.

use std::ops::Range;

use ae_span::Diagnostic;

use crate::expression::{parse_expression_tokens, parse_expression_tokens_no_struct, Expression};
use crate::operator::infix_binding_power;
use crate::{diagnostic_range, Kind, Token};

/// One block or one statement body. `range` covers the whole construct,
/// including its braces where it has them.
#[derive(Debug, PartialEq)]
pub struct Statement {
    pub kind: StatementKind,
    pub range: Range<usize>,
}

#[derive(Debug, PartialEq)]
pub enum StatementKind {
    /// `let name = init;` / `var name = init;`. The initializer may carry a
    /// trailing `?` (already folded into `ExpressionKind::Propagate`).
    Binding { mutable: bool, name: String, name_range: Range<usize>, initializer: Expression },
    /// `target = value;`, `target += value;`, `name[i] = value;`,
    /// `name.field = value;`. The target is an expression because the grammar
    /// cannot tell a variable from a call without resolving names; the
    /// resolver decides whether the target is writable.
    Assign { target: Expression, operator: Kind, value: Expression },
    /// `*pointer = value;` — the only statement form whose left side is not an
    /// expression (a bare `*` is not a prefix operator).
    PointerStore { pointer: String, pointer_range: Range<usize>, value: Expression },
    Print(Expression),
    Assert(Expression),
    Return(Expression),
    If { condition: Expression, then_body: Vec<Statement>, else_body: Option<ElseBranch> },
    While { condition: Expression, body: Vec<Statement> },
    /// `for name in start..end { .. }`
    ForRange { binding: String, binding_range: Range<usize>, start: Expression, end: Expression, body: Vec<Statement> },
    /// `for name in iterable { .. }` — a vector, a string view, or a cursor.
    ForIn { binding: String, binding_range: Range<usize>, iterable: Expression, body: Vec<Statement> },
    Match { subject: Expression, arms: Vec<MatchArm> },
    Break,
    Continue,
    Unsafe(Vec<Statement>),
    /// A call used for its effect. The compiler requires a trailing `?`; the
    /// grammar keeps the flag so the resolver can report it.
    Expr { value: Expression, propagates: bool },
}

#[derive(Debug, PartialEq)]
pub enum ElseBranch {
    Block(Vec<Statement>),
    If(Box<Statement>),
}

#[derive(Debug, PartialEq)]
pub struct MatchArm {
    pub pattern: crate::pattern::Pattern,
    pub body: Vec<Statement>,
    pub range: Range<usize>,
}

/// Parse `{ statement* }` starting at the `{` token.
pub fn parse_block(file: &str, source: &str, tokens: &[Token], start: usize) -> Result<(Vec<Statement>, usize), Diagnostic> {
    let mut parser = Parser { file, source, tokens, pos: start, statement_depth: 0 };
    let body = parser.block()?;
    Ok((body, parser.pos))
}

/// Parse the statements of a function body whose `{` has already been consumed.
pub fn parse_statements_until_close(file: &str, source: &str, tokens: &[Token], start: usize) -> Result<(Vec<Statement>, usize), Diagnostic> {
    let mut parser = Parser { file, source, tokens, pos: start, statement_depth: 0 };
    let body = parser.statements()?;
    Ok((body, parser.pos))
}

/// Parse exactly one statement at `start`, returning it and the first
/// unconsumed token. The compiler uses this as a boundary oracle while its own
/// statement parser is still the one building the typed tree (ADR-0224 R3a).
pub fn parse_statement_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<(Statement, usize), Diagnostic> {
    let mut parser = Parser { file, source, tokens, pos: start, statement_depth: 0 };
    let statement = parser.statement()?;
    Ok((statement, parser.pos))
}

/// The declaration portion of a local binding, before its initializer.
pub struct BindingHeader<'a> {
    pub name: &'a str,
    pub name_range: Range<usize>,
    pub mutable: bool,
}

/// Read a `let` or `var` header from the keyword token and stop before `=`.
/// The compiler checks scope and writability before reading the initializer.
pub fn parse_binding_header<'a>(
    file: &str,
    source: &str,
    tokens: &'a [Token],
    start: usize,
) -> Result<BindingHeader<'a>, Diagnostic> {
    let token = &tokens[start + 1];
    Ok(BindingHeader {
        name: crate::parse_identifier(file, source, token)?,
        name_range: token.offset..token.end,
        mutable: tokens[start].kind == Kind::Var,
    })
}

struct Parser<'a> {
    file: &'a str,
    source: &'a str,
    tokens: &'a [Token],
    pos: usize,
    statement_depth: usize,
}

impl Parser<'_> {
    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn error(&self, message: &str) -> Diagnostic {
        let token = self.current();
        diagnostic_range(self.file, self.source, token.offset..token.end, message)
    }

    fn take(&mut self, expected: Kind) -> Result<&Token, Diagnostic> {
        if self.current().kind != expected {
            return Err(self.error(&format!("expected {:?}, found {:?}", expected, self.current().kind)).with_code("E0010"));
        }
        self.pos += 1;
        Ok(&self.tokens[self.pos - 1])
    }

    fn identifier(&mut self) -> Result<(String, Range<usize>), Diagnostic> {
        let token = self.current();
        let Kind::Ident(name) = &token.kind else {
            return Err(self.error("expected identifier").with_code("E0010"));
        };
        let value = name.clone();
        let range = token.offset..token.end;
        self.pos += 1;
        Ok((value, range))
    }

    fn block(&mut self) -> Result<Vec<Statement>, Diagnostic> {
        self.take(Kind::LBrace)?;
        let body = self.statements()?;
        if self.current().kind != Kind::RBrace {
            return Err(self.error("expected '}' to close the block").with_code("E0010"));
        }
        self.pos += 1;
        Ok(body)
    }

    fn statements(&mut self) -> Result<Vec<Statement>, Diagnostic> {
        let mut body = Vec::new();
        while !matches!(self.current().kind, Kind::RBrace | Kind::Eof) {
            body.push(self.statement()?);
        }
        Ok(body)
    }

    fn statement(&mut self) -> Result<Statement, Diagnostic> {
        if self.statement_depth >= 64 {
            return Err(self.error("statement nesting limit exceeded (64)"));
        }
        self.statement_depth += 1;
        let result = self.statement_inner();
        self.statement_depth -= 1;
        result
    }

    fn expression(&mut self, propagate: bool, allow_struct: bool) -> Result<Expression, Diagnostic> {
        let parse = if allow_struct { parse_expression_tokens } else { parse_expression_tokens_no_struct };
        let (value, end) = parse(self.file, self.source, self.tokens, self.pos, propagate)?;
        self.pos = end;
        Ok(value)
    }

    fn statement_inner(&mut self) -> Result<Statement, Diagnostic> {
        let start = self.current().offset;
        let kind = match self.current().kind.clone() {
            Kind::If => {
                let (condition, then_body, else_body) = self.if_chain()?;
                StatementKind::If { condition, then_body, else_body }
            }
            Kind::While => {
                self.pos += 1;
                let condition = self.expression(false, false)?;
                let body = self.block()?;
                StatementKind::While { condition, body }
            }
            Kind::For => {
                self.pos += 1;
                let (binding, binding_range) = self.identifier()?;
                self.take(Kind::In)?;
                let first = self.expression(false, false)?;
                if self.current().kind == Kind::DotDot {
                    self.pos += 1;
                    // The range end sits in the same condition-like position:
                    // the `{` after it opens the loop body.
                    let last = self.expression(false, false)?;
                    let body = self.block()?;
                    StatementKind::ForRange { binding, binding_range, start: first, end: last, body }
                } else {
                    let body = self.block()?;
                    StatementKind::ForIn { binding, binding_range, iterable: first, body }
                }
            }
            Kind::Match => {
                let (subject, arms) = self.match_statement()?;
                StatementKind::Match { subject, arms }
            }
            Kind::Unsafe => {
                self.pos += 1;
                let body = self.block()?;
                StatementKind::Unsafe(body)
            }
            _ => self.simple_statement()?,
        };
        Ok(Statement { kind, range: start..self.tokens[self.pos - 1].end })
    }

    fn simple_statement(&mut self) -> Result<StatementKind, Diagnostic> {
        Ok(match self.current().kind {
            Kind::Let | Kind::Var => {
                let BindingHeader { name, name_range, mutable } =
                    parse_binding_header(self.file, self.source, self.tokens, self.pos)?;
                self.pos += 2;
                self.take(Kind::Equal)?;
                let initializer = self.expression(true, true)?;
                self.take(Kind::Semi)?;
                StatementKind::Binding { mutable, name: name.to_owned(), name_range, initializer }
            }
            Kind::Print => {
                self.pos += 1;
                self.take(Kind::LParen)?;
                let value = self.expression(false, true)?;
                self.take(Kind::RParen)?;
                self.take(Kind::Semi)?;
                StatementKind::Print(value)
            }
            Kind::Assert => {
                self.pos += 1;
                self.take(Kind::LParen)?;
                let value = self.expression(false, true)?;
                self.take(Kind::RParen)?;
                self.take(Kind::Semi)?;
                StatementKind::Assert(value)
            }
            Kind::Return => {
                self.pos += 1;
                let value = if self.current().kind == Kind::Semi {
                    let start = self.tokens[self.pos - 1].offset;
                    let end = self.current().offset;
                    Expression { kind: crate::expression::ExpressionKind::Unit, range: start..end }
                } else {
                    self.expression(true, true)?
                };
                self.take(Kind::Semi)?;
                StatementKind::Return(value)
            }
            Kind::Break => {
                self.pos += 1;
                self.take(Kind::Semi)?;
                StatementKind::Break
            }
            Kind::Continue => {
                self.pos += 1;
                self.take(Kind::Semi)?;
                StatementKind::Continue
            }
            Kind::Star => {
                self.pos += 1;
                let (pointer, pointer_range) = self.identifier()?;
                self.take(Kind::Equal)?;
                let value = self.expression(false, true)?;
                self.take(Kind::Semi)?;
                StatementKind::PointerStore { pointer, pointer_range, value }
            }
            _ => {
                let value = self.expression(true, true)?;
                // A compound or plain assignment: the target was parsed as an
                // expression so `name[i]`, `name.field` and `name` share one
                // grammar; the resolver checks writability.
                if matches!(self.current().kind, Kind::Equal | Kind::PlusEqual | Kind::MinusEqual | Kind::StarEqual | Kind::SlashEqual | Kind::PercentEqual) {
                    let operator = self.current().kind.clone();
                    self.pos += 1;
                    let rhs = self.expression(false, true)?;
                    self.take(Kind::Semi)?;
                    StatementKind::Assign { target: value, operator, value: rhs }
                } else {
                    let propagates = matches!(value.kind, crate::expression::ExpressionKind::Propagate(_));
                    self.take(Kind::Semi)?;
                    StatementKind::Expr { value, propagates }
                }
            }
        })
    }

    fn if_chain(&mut self) -> Result<(Expression, Vec<Statement>, Option<ElseBranch>), Diagnostic> {
        self.take(Kind::If)?;
        let condition = self.expression(false, false)?;
        let then_body = self.block()?;
        let else_body = if self.current().kind == Kind::Else {
            self.pos += 1;
            if self.current().kind == Kind::If {
                Some(ElseBranch::If(Box::new(self.statement()?)))
            } else {
                Some(ElseBranch::Block(self.block()?))
            }
        } else {
            None
        };
        Ok((condition, then_body, else_body))
    }

    fn match_statement(&mut self) -> Result<(Expression, Vec<MatchArm>), Diagnostic> {
        self.take(Kind::Match)?;
        let subject = self.expression(false, false)?;
        self.take(Kind::LBrace)?;
        let mut arms = Vec::new();
        while self.current().kind != Kind::RBrace {
            let start = self.current().offset;
            let (pattern, end) = crate::pattern::parse_pattern_tokens(self.file, self.source, self.tokens, self.pos)?;
            self.pos = end;
            self.take(Kind::FatArrow)?;
            let body = self.block()?;
            arms.push(MatchArm { pattern, body, range: start..self.tokens[self.pos - 1].end });
        }
        self.take(Kind::RBrace)?;
        Ok((subject, arms))
    }
}

/// Visit a statement and its nested statements in source order.
pub fn visit_statements(statement: &Statement, visit: &mut impl FnMut(&Statement)) {
    visit(statement);
    match &statement.kind {
        StatementKind::If { then_body, else_body, .. } => {
            then_body.iter().for_each(|inner| visit_statements(inner, visit));
            match else_body {
                Some(ElseBranch::Block(body)) => body.iter().for_each(|inner| visit_statements(inner, visit)),
                Some(ElseBranch::If(inner)) => visit_statements(inner, visit),
                None => {}
            }
        }
        StatementKind::While { body, .. }
        | StatementKind::ForRange { body, .. }
        | StatementKind::ForIn { body, .. }
        | StatementKind::Unsafe(body) => body.iter().for_each(|inner| visit_statements(inner, visit)),
        StatementKind::Match { arms, .. } => {
            arms.iter().for_each(|arm| arm.body.iter().for_each(|inner| visit_statements(inner, visit)));
        }
        _ => {}
    }
}

impl Statement {
    /// Expressions directly owned by this statement, excluding nested bodies.
    pub fn expressions(&self) -> impl Iterator<Item = &Expression> {
        let pair = match &self.kind {
            StatementKind::Binding { initializer, .. } => [Some(initializer), None],
            StatementKind::Assign { target, value, .. } => [Some(target), Some(value)],
            StatementKind::PointerStore { value, .. }
            | StatementKind::Print(value)
            | StatementKind::Assert(value)
            | StatementKind::Return(value)
            | StatementKind::Expr { value, .. } => [Some(value), None],
            StatementKind::If { condition, .. }
            | StatementKind::While { condition, .. } => [Some(condition), None],
            StatementKind::ForRange { start, end, .. } => [Some(start), Some(end)],
            StatementKind::ForIn { iterable, .. } => [Some(iterable), None],
            StatementKind::Match { subject, .. } => [Some(subject), None],
            StatementKind::Unsafe(_) | StatementKind::Break | StatementKind::Continue => [None, None],
        };
        pair.into_iter().flatten()
    }
}

/// Depth-first walk over the expression roots owned by this statement and its body.
pub fn visit_expressions(statement: &Statement, visit: &mut impl FnMut(&Expression)) {
    visit_statements(statement, &mut |statement| statement.expressions().for_each(&mut *visit));
}

/// True when a binary operator at this position could have continued the
/// expression. Kept next to the grammar so a future statement splitter and the
/// expression parser cannot disagree about where an expression stops.
pub fn continues_expression(kind: &Kind) -> bool {
    infix_binding_power(kind).is_some()
}

/// Builtin calls the compiler accepts in statement position without a `?`.
/// These are the effectful library/builtin statements (`push(v, x);`,
/// `chan_send(ch, v);`, `exit(1);`, …); every other bare call must propagate
/// its result. The resolver still owns the argument and effect checks.
pub const STATEMENT_BUILTINS: &[&str] = &[
    "exit", "cell_store", "shared_set", "chan_close", "task_spawn", "task_yield",
    "scope_spawn", "pool_spawn", "push", "pop", "sort", "reverse",
];

impl StatementKind {
    /// True for a call the compiler accepts as a statement on its own
    /// (propagating calls, statement builtins, and bare unit-returning calls).
    pub fn is_effect_statement(&self) -> bool {
        let StatementKind::Expr { value, propagates } = self else { return false };
        if *propagates {
            return true;
        }
        matches!(&value.kind, crate::expression::ExpressionKind::Call { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_nested_blocks_and_else_if_chains() {
        for (allowed, excessive) in [
            (
                format!("{{ {}{} }}", "while true { ".repeat(64), "}".repeat(64)),
                format!("{{ {}{} }}", "while true { ".repeat(65), "}".repeat(65)),
            ),
            (
                format!("{{ if true {{}} {} }}", "else if true {} ".repeat(63)),
                format!("{{ if true {{}} {} }}", "else if true {} ".repeat(64)),
            ),
        ] {
            let allowed_tokens = crate::lex_diagnostic(&allowed).unwrap();
            assert!(parse_block("case.ae", &allowed, &allowed_tokens, 0).is_ok());
            let excessive_tokens = crate::lex_diagnostic(&excessive).unwrap();
            let error = parse_block("case.ae", &excessive, &excessive_tokens, 0).unwrap_err();
            assert_eq!(error.message, "statement nesting limit exceeded (64)");
        }
    }

    fn body(source: &str) -> Vec<Statement> {
        parse_body(&format!("{{ {source} }}"))
    }

    fn parse_body(source: &str) -> Vec<Statement> {
        let tokens = crate::lex_diagnostic(source).unwrap();
        let (body, end) = parse_block("case.ae", source, &tokens, 0).unwrap();
        assert_eq!(tokens[end].kind, Kind::Eof, "the block consumes its closing brace");
        body
    }

    #[test]
    fn bindings_keep_mutability_names_and_ranges() {
        let source = "let name = fetch()?; var count = 0;";
        let wrapped = format!("{{ {source} }}");
        let statements = parse_body(&wrapped);
        assert_eq!(statements.len(), 2);
        let StatementKind::Binding { mutable, name, name_range, initializer } = &statements[0].kind else {
            panic!("expected a binding");
        };
        assert!(!mutable);
        assert_eq!(name, "name");
        assert_eq!(&wrapped[name_range.clone()], "name");
        assert_eq!(&wrapped[initializer.range.clone()], "fetch()?");
        assert!(matches!(initializer.kind, crate::expression::ExpressionKind::Propagate(_)));
        let StatementKind::Binding { mutable, name, .. } = &statements[1].kind else {
            panic!("expected a binding");
        };
        assert!(mutable);
        assert_eq!(name, "count");
    }

    #[test]
    fn assignments_and_pointer_stores_are_distinct() {
        let source = "x = 1; y += 2; v[i] = 3; s.f = 4; *p = 5;";
        let statements = body(source);
        assert_eq!(statements.len(), 5);
        for (index, expected) in [Kind::Equal, Kind::PlusEqual, Kind::Equal, Kind::Equal].into_iter().enumerate() {
            let StatementKind::Assign { operator, .. } = &statements[index].kind else {
                panic!("statement {index} should be an assignment");
            };
            assert_eq!(*operator, expected);
        }
        assert!(matches!(statements[4].kind, StatementKind::PointerStore { .. }));
    }

    #[test]
    fn calls_without_propagation_are_recorded_as_such() {
        let statements = body("helper(); save(x)?;");
        assert!(matches!(&statements[0].kind, StatementKind::Expr { propagates: false, .. }));
        assert!(matches!(&statements[1].kind, StatementKind::Expr { propagates: true, .. }));
    }

    #[test]
    fn control_flow_shapes_nest_without_losing_ranges() {
        let source = "if a { let x = 1; } else if b { let y = 2; } else { let z = 3; }";
        let statements = body(source);
        let StatementKind::If { then_body, else_body, .. } = &statements[0].kind else {
            panic!("expected an if");
        };
        assert_eq!(then_body.len(), 1);
        let Some(ElseBranch::If(nested)) = else_body else {
            panic!("else-if must recurse, not nest a block");
        };
        let StatementKind::If { else_body: Some(ElseBranch::Block(inner)), .. } = &nested.kind else {
            panic!("the final else keeps a block");
        };
        assert_eq!(inner.len(), 1);
    }

    #[test]
    fn loops_keep_the_range_and_container_forms_apart() {
        let statements = body("for i in 0..n { print(i); } for item in items { print(item); } while a { break; }");
        assert!(matches!(statements[0].kind, StatementKind::ForRange { .. }));
        assert!(matches!(statements[1].kind, StatementKind::ForIn { .. }));
        let StatementKind::While { body, .. } = &statements[2].kind else {
            panic!("expected a while");
        };
        assert!(matches!(body[0].kind, StatementKind::Break));
    }

    #[test]
    fn match_arms_reuse_the_shared_pattern_grammar() {
        let source = "match shape { Shape::Rect(w, h) => { print(w); } Shape::Empty => { } }";
        let statements = body(source);
        let StatementKind::Match { arms, .. } = &statements[0].kind else {
            panic!("expected a match");
        };
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0].body.len(), 1);
        assert!(arms[1].body.is_empty());
    }

    #[test]
    fn unsafe_blocks_and_print_assert_return_keep_their_values() {
        let statements = body("unsafe { *p = 1; } print(1); assert(true); return 2;");
        assert!(matches!(&statements[0].kind, StatementKind::Unsafe(inner) if inner.len() == 1));
        assert!(matches!(statements[1].kind, StatementKind::Print(_)));
        assert!(matches!(statements[2].kind, StatementKind::Assert(_)));
        assert!(matches!(statements[3].kind, StatementKind::Return(_)));
    }

    #[test]
    fn visit_expressions_reaches_every_nested_statement() {
        let statements = body("if a { print(b); } else { return c; } while d { assert(e); }");
        let mut seen = 0;
        for statement in &statements {
            visit_expressions(statement, &mut |_| seen += 1);
        }
        assert_eq!(seen, 5, "if, print, return, while and assert each contribute one expression");
    }

    #[test]
    fn a_statement_cannot_swallow_the_next_one() {
        let statements = body("let a = 1; let b = 2;");
        assert_eq!(statements.len(), 2);
        let source = "{ let a = ; }";
        let tokens = crate::lex_diagnostic(source).unwrap();
        let failure = parse_block("case.ae", source, &tokens, 0).unwrap_err();
        assert_eq!(failure.code.as_deref(), Some("E0011"));
        assert!(failure.message.contains("expected expression"));
    }
}
