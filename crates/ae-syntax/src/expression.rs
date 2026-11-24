use std::ops::Range;

use ae_span::Diagnostic;

use crate::literal::parse_literal;
use crate::operator::{parse_infix_tail, prefix_binding_power};
use crate::{diagnostic_range, Kind, Token};

#[derive(Clone, Debug, PartialEq)]
pub struct Expression {
    pub kind: ExpressionKind,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInitializer {
    pub name: String,
    pub name_range: Range<usize>,
    pub value: Expression,
}

/// One `name: Type` in a closure's parameter list. The type stays a byte
/// range: the shared parser records the spelling, the compiler resolves it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosureParam {
    pub name: String,
    pub name_range: Range<usize>,
    pub ty: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExpressionKind {
    Atom(Kind),
    Group(Box<Expression>),
    Propagate(Box<Expression>),
    AddressCast { binding: String, binding_range: Range<usize>, mutable: bool, pointee: Range<usize> },
    PointerCast { base: Box<Expression>, mutable: bool, pointee: Range<usize> },
    /// `base as TypeName` for a scalar spelling (`i64`, `f64`, ...). The
    /// compiler owns the cast semantics; the grammar keeps the target range.
    ScalarCast { base: Box<Expression>, target: Range<usize> },
    BorrowArgument { mutable: bool, name: String, name_range: Range<usize> },
    Array(Vec<Expression>),
    Vector(Vec<Expression>),
    Struct { target: Box<Expression>, fields: Vec<FieldInitializer> },
    Call { callee: Box<Expression>, arguments: Vec<Expression> },
    Index { base: Box<Expression>, index: Box<Expression> },
    Field { base: Box<Expression>, name: String },
    Specialization { base: Box<Expression>, arguments: Vec<Range<usize>> },
    // A name followed by brackets and a call can denote specialization or
    // indexing. The resolver can use these ranges once the name is known.
    BracketedCallTarget { base: Box<Expression>, arguments: Vec<Range<usize>> },
    Qualified { base: Box<Expression>, name: String },
    /// `|name: Type, ...| -> Ret { ... }` / `|| -> Ret { ... }`. The body is
    /// an opaque brace-balanced range: statement parsing stays with the
    /// caller, capture inference with the compiler.
    Closure { params: Vec<ClosureParam>, return_type: Range<usize>, body: Range<usize> },
    Prefix { operator: Kind, operand: Box<Expression> },
    Binary { left: Box<Expression>, operator: Kind, right: Box<Expression> },
}

/// Parse operator expressions with calls, indexing, and field access.
/// Collection initializers retain their element syntax. The compiler checks
/// name resolution, array placement, and element types.
pub fn parse_operator_expression(file: &str, source: &str) -> Result<Expression, Diagnostic> {
    let tokens = crate::lex_diagnostic(source).map_err(|mut diagnostic| {
        if let Some(label) = &mut diagnostic.primary {
            label.span.file = Some(file.to_owned());
        }
        diagnostic
    })?;
    let (expression, end) = parse_expression_tokens(file, source, &tokens, 0, true)?;
    let token = &tokens[end];
    if token.kind != Kind::Eof {
        return Err(diagnostic_range(file, source, token.offset..token.end, "expected end of expression"));
    }
    Ok(expression)
}

/// Read one expression from an EOF-terminated lexer stream, returning the
/// first unconsumed token. Statement callers can opt into a final '?'.
pub fn parse_expression_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    allow_propagation: bool,
) -> Result<(Expression, usize), Diagnostic> {
    parse_expression_with(file, source, tokens, start, allow_propagation, false)
}

/// Like [`parse_expression_tokens`], but a trailing `{` never opens a struct
/// literal. Statement callers use this for a condition or an iterable: the
/// brace there belongs to the block that follows (`if flag { ... }`), and the
/// compiler resolves the same way. Rust calls this the "no struct literal in a
/// condition" restriction; without it `if a { .. }` parses as `a { .. }`.
pub fn parse_expression_tokens_no_struct(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    allow_propagation: bool,
) -> Result<(Expression, usize), Diagnostic> {
    parse_expression_with(file, source, tokens, start, allow_propagation, true)
}

fn parse_expression_with(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    allow_propagation: bool,
    no_struct: bool,
) -> Result<(Expression, usize), Diagnostic> {
    let mut parser = Parser { file, source, tokens, cursor: start, nodes: 0, no_struct };
    let mut expression = parser.expression(0)?;
    if allow_propagation && parser.current().kind == Kind::Question {
        let range = expression.range.start..parser.current().end;
        parser.cursor += 1;
        expression = Expression { kind: ExpressionKind::Propagate(Box::new(expression)), range };
    }
    Ok((expression, parser.cursor))
}

/// Parse a borrowed call argument at an existing token cursor.
pub fn parse_borrow_argument(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    required_mutability: Option<bool>,
) -> Result<(Expression, usize), Diagnostic> {
    let error = |token: &Token, message: &str| {
        diagnostic_range(file, source, token.offset..token.end, message)
    };
    let amp = &tokens[start];
    if amp.kind != Kind::Amp {
        return Err(error(amp, &format!("expected Amp, found {:?}", amp.kind)).with_code("E0010"));
    }
    let marker = &tokens[start + 1];
    let mutable = marker.kind == Kind::Mut && required_mutability != Some(false);
    if required_mutability == Some(true) && !mutable {
        return Err(error(marker, "this parameter borrows mutably; write the argument as '&mut name'"));
    }
    let index = start + 1 + usize::from(mutable);
    let token = &tokens[index];
    let Kind::Ident(name) = &token.kind else {
        return Err(error(token, "expected identifier").with_code("E0010"));
    };
    let kind = ExpressionKind::BorrowArgument {
        mutable,
        name: name.clone(),
        name_range: token.offset..token.end,
    };
    Ok((Expression { kind, range: amp.offset..token.end }, index + 1))
}

struct Parser<'a> {
    file: &'a str,
    source: &'a str,
    tokens: &'a [Token],
    cursor: usize,
    nodes: usize,
    /// True inside a statement's condition or iterable position: a `{` there
    /// opens the statement body, never a struct literal.
    no_struct: bool,
}

impl Parser<'_> {
    fn current(&self) -> &Token {
        &self.tokens[self.cursor]
    }

    fn error(&self, message: &str) -> Diagnostic {
        let token = self.current();
        diagnostic_range(self.file, self.source, token.offset..token.end, message)
    }

    fn error_at(&self, index: usize, message: &str) -> Diagnostic {
        let token = &self.tokens[index];
        diagnostic_range(self.file, self.source, token.offset..token.end, message)
    }

    fn count_node(&mut self) -> Result<(), Diagnostic> {
        if self.nodes == 128 {
            return Err(self.error("expression complexity limit exceeded (128)"));
        }
        self.nodes += 1;
        Ok(())
    }

    fn expression(&mut self, minimum: u8) -> Result<Expression, Diagnostic> {
        self.count_node()?;
        // The restriction is positional: it applies to the top-level operand
        // of a condition, and is lifted inside any bracketed subexpression
        // (`if (Pair { a: 1 }).a == 1`). A binary condition keeps it, because
        // the trailing `{` belongs to the body (`if x > best { .. }`).
        let no_struct = self.no_struct;
        self.no_struct = false;
        let token = &self.tokens[self.cursor];
        let mut range = token.offset..token.end;
        let kind = if let Some(power) = prefix_binding_power(&token.kind) {
            self.cursor += 1;
            // A prefix operator does not end the condition: `!flag {` and
            // `-x < limit {` still have the body's `{` next.
            self.no_struct = no_struct;
            let operand = self.expression(power)?;
            range.end = operand.range.end;
            ExpressionKind::Prefix { operator: token.kind.clone(), operand: Box::new(operand) }
        } else {
            match &token.kind {
                Kind::Amp => {
                    let (argument, end) = parse_borrow_argument(
                        self.file, self.source, self.tokens, self.cursor, None,
                    )?;
                    self.cursor = end;
                    let ExpressionKind::BorrowArgument { name, name_range, mutable } = argument.kind else {
                        unreachable!("expected a borrowed binding");
                    };
                    if self.current().kind != Kind::As {
                        return Err(self.error("a borrow expression must be cast to a pointer"));
                    }
                    self.cursor += 1;
                    let (target_mutable, pointee) = self.pointer_cast_target()?;
                    if mutable != target_mutable {
                        return Err(self.error("pointer qualifier must match the address borrow"));
                    }
                    range.end = pointee.end;
                    ExpressionKind::AddressCast { binding: name, binding_range: name_range, mutable, pointee }
                }
                Kind::LParen => {
                    self.cursor += 1;
                    let inner = self.expression(0)?;
                    crate::expect_token(self.file, self.source, self.current(), Kind::RParen)?;
                    range.end = self.current().end;
                    self.cursor += 1;
                    ExpressionKind::Group(Box::new(inner))
                }
                Kind::LBracket => {
                    let elements = self.collection(false)?;
                    range.end = self.tokens[self.cursor - 1].end;
                    ExpressionKind::Array(elements)
                }
                Kind::Pipe => {
                    self.cursor += 1;
                    let kind = self.closure(false)?;
                    range.end = self.tokens[self.cursor - 1].end;
                    kind
                }
                Kind::Or if self.tokens.get(self.cursor + 1).is_some_and(|token| token.kind == Kind::Arrow) => {
                    self.cursor += 1;
                    let kind = self.closure(true)?;
                    range.end = self.tokens[self.cursor - 1].end;
                    kind
                }
                Kind::Ident(name) if name == "vec"
                    && self.tokens.get(self.cursor + 1).is_some_and(|token| token.kind == Kind::LBracket) =>
                {
                    self.cursor += 1;
                    let elements = self.collection(true)?;
                    range.end = self.tokens[self.cursor - 1].end;
                    ExpressionKind::Vector(elements)
                }
                Kind::Ident(_) => {
                    self.cursor += 1;
                    ExpressionKind::Atom(token.kind.clone())
                }
                _ if parse_literal(token).is_some() => {
                    self.cursor += 1;
                    ExpressionKind::Atom(token.kind.clone())
                }
                // The typed route's wording and code: these fixtures pin it
                // through `check --json`, and the two parsers must agree.
                _ => return Err(self.error("expected expression").with_code("E0011")),
            }
        };
        let left = self.postfix(Expression { kind, range }, no_struct)?;
        // Restore the flag for the binary continuation only: its right operand
        // is still the same condition, so `x > best {` must stop at `{`.
        self.no_struct = no_struct;
        let combined = parse_infix_tail(self, left, minimum, |parser| &parser.current().kind,
            |parser, left, operator, right_power| {
                parser.cursor += 1;
                let right = parser.expression(right_power)?;
                Ok(Expression {
                    range: left.range.start..right.range.end,
                    kind: ExpressionKind::Binary { left: Box::new(left), operator, right: Box::new(right) },
                })
            });
        // The tail's right operands each consumed their own copy of the
        // flag; a sibling operator that follows (`b > 0 && a > x - b {`)
        // still sits in the same condition position, so the entry value —
        // not `false` — is what the caller's continuation must see.
        self.no_struct = no_struct;
        combined
    }
    // The caller consumed the opener (a `|` with parameters, or the `||`
    // shorthand). Parameter types and the return type stay byte ranges —
    // the compiler resolves spellings and capture modes against its scopes.
    fn closure(&mut self, empty_params: bool) -> Result<ExpressionKind, Diagnostic> {
        let mut params = Vec::new();
        if !empty_params {
            loop {
                if params.len() == 16 {
                    return Err(self.error("closure parameter limit exceeded (16)"));
                }
                let token = self.current();
                let name_range = token.offset..token.end;
                let name = crate::parse_identifier(self.file, self.source, token)?.to_owned();
                self.cursor += 1;
                crate::expect_token(self.file, self.source, self.current(), Kind::Colon)?;
                self.cursor += 1;
                let (ty, end) = crate::declaration::type_spelling(
                    self.file, self.source, self.tokens, self.cursor, &[Kind::Comma, Kind::Pipe],
                )?;
                self.cursor = end;
                params.push(ClosureParam { name, name_range, ty });
                if self.current().kind != Kind::Comma {
                    break;
                }
                self.cursor += 1;
            }
            crate::expect_token(self.file, self.source, self.current(), Kind::Pipe)?;
            self.cursor += 1;
        }
        crate::expect_token(self.file, self.source, self.current(), Kind::Arrow)?;
        self.cursor += 1;
        let (return_type, end) = crate::declaration::type_spelling(
            self.file, self.source, self.tokens, self.cursor, &[Kind::LBrace],
        )?;
        self.cursor = end;
        crate::expect_token(self.file, self.source, self.current(), Kind::LBrace)?;
        let body_start = self.cursor;
        let body_end = crate::declaration::brace_body_end(self.tokens, body_start + 1)
            .map_err(|at| self.error_at(at, "expected '}' to close the closure body"))?;
        let body = self.tokens[body_start].offset..self.tokens[body_end - 1].end;
        self.cursor = body_end;
        Ok(ExpressionKind::Closure { params, return_type, body })
    }

    fn collection(&mut self, allow_empty: bool) -> Result<Vec<Expression>, Diagnostic> {
        self.cursor += 1;
        let elements = if self.current().kind == Kind::RBracket {
            if !allow_empty {
                return Err(self.error("array must contain at least one element"));
            }
            Vec::new()
        } else {
            crate::parse_comma_list(self, |parser, _| parser.expression(0), |parser| {
                if parser.current().kind != Kind::Comma {
                    return false;
                }
                parser.cursor += 1;
                true
            })?
        };
        crate::expect_token(self.file, self.source, self.current(), Kind::RBracket)?;
        self.cursor += 1;
        Ok(elements)
    }

    fn index(&mut self, base: Expression) -> Result<ExpressionKind, Diagnostic> {
        let index = self.expression(0)?;
        crate::expect_token(self.file, self.source, self.current(), Kind::RBracket)?;
        self.cursor += 1;
        Ok(ExpressionKind::Index { base: Box::new(base), index: Box::new(index) })
    }

    fn struct_fields(&mut self) -> Result<Vec<FieldInitializer>, Diagnostic> {
        let mut fields = Vec::new();
        while self.current().kind != Kind::RBrace {
            let token = self.current();
            let name = crate::parse_identifier(self.file, self.source, token)?.to_owned();
            let name_range = token.offset..token.end;
            self.cursor += 1;
            if self.current().kind != Kind::Colon {
                return Err(self.error("expected ':' after struct field name"));
            }
            self.cursor += 1;
            let value = self.expression(0)?;
            fields.push(FieldInitializer { name, name_range, value });
            if self.current().kind == Kind::Comma {
                self.cursor += 1;
            }
        }
        self.cursor += 1;
        Ok(fields)
    }

    fn call_argument(&mut self) -> Result<Expression, Diagnostic> {
        if self.current().kind != Kind::Amp {
            return self.expression(0);
        }
        let name_index = self.cursor + 1 + usize::from(self.tokens[self.cursor + 1].kind == Kind::Mut);
        if self.tokens.get(name_index + 1).is_some_and(|token| token.kind == Kind::As) {
            return self.expression(0);
        }
        self.count_node()?;
        let (argument, end) = parse_borrow_argument(self.file, self.source, self.tokens, self.cursor, None)?;
        self.cursor = end;
        Ok(argument)
    }

    fn pointer_cast_target(&mut self) -> Result<(bool, Range<usize>), Diagnostic> {
        if self.current().kind != Kind::Star {
            return Err(self.error("expected '*' after 'as'"));
        }
        self.cursor += 1;
        let mutable = crate::declaration::pointer_mutability(
            self.file, self.source, self.current(),
        )?;
        self.cursor += 1;
        if !matches!(self.current().kind, Kind::Ident(_)) {
            return Err(self.error("expected a pointee type"));
        }
        let start = self.current().offset;
        self.cursor += 1;
        if self.current().kind == Kind::LBracket {
            let (_, end) = crate::declaration::expression_type_arguments(
                self.file, self.source, self.tokens, self.cursor,
            )?;
            self.cursor = end;
        }
        Ok((mutable, start..self.tokens[self.cursor - 1].end))
    }

    fn postfix(&mut self, mut base: Expression, no_struct: bool) -> Result<Expression, Diagnostic> {
        // `[` continues the postfix chain only after a name or a field of one
        // (the typed parser's index rule); any other base leaves the bracket
        // for the caller's own diagnostic (`expected RParen, found LBracket`).
        while matches!(self.current().kind, Kind::LParen | Kind::Dot | Kind::ColonColon | Kind::As)
            || (self.current().kind == Kind::LBracket
                && matches!(&base.kind, ExpressionKind::Atom(Kind::Ident(_)) | ExpressionKind::Field { .. }))
            || (!no_struct && self.current().kind == Kind::LBrace
                && matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)) | ExpressionKind::Specialization { .. }))
        {
            self.count_node()?;
            let start = base.range.start;
            let opener = &self.tokens[self.cursor].kind;
            self.cursor += 1;
            let kind = match opener {
                Kind::LParen => {
                    let arguments = if self.current().kind == Kind::RParen {
                        Vec::new()
                    } else {
                        crate::parse_comma_list(self, |parser, _| parser.call_argument(), |parser| {
                            if parser.current().kind != Kind::Comma {
                                return false;
                            }
                            parser.cursor += 1;
                            true
                        })?
                    };
                    crate::expect_token(self.file, self.source, self.current(), Kind::RParen)?;
                    self.cursor += 1;
                    ExpressionKind::Call { callee: Box::new(base), arguments }
                }
                Kind::LBracket if matches!(&base.kind, ExpressionKind::Atom(Kind::Ident(_))) => {
                    let start = self.cursor - 1;
                    let end = crate::declaration::skip_type_parameter_list(self.tokens, start);
                    if self.tokens.get(end).is_some_and(|token| matches!(token.kind, Kind::ColonColon | Kind::LBrace)) {
                        // `Name[...] ::` / `Name[...] {` is a generic
                        // instantiation (`Pair[string, i64] { ... }`,
                        // `Option[i64]::Some`) — unless the bracket contents
                        // are not a type argument list, in which case this
                        // was an index whose value a brace follows
                        // (`if arr[i + 1] {`). Names are the only authority
                        // here, and the grammar has none, so a non-type
                        // spelling falls back to indexing and the caller
                        // owns the brace (ADR-0228's grammar survey).
                        match crate::declaration::expression_type_arguments(
                            self.file, self.source, self.tokens, start,
                        ) {
                            Ok((arguments, args_end)) => {
                                self.cursor = args_end;
                                ExpressionKind::Specialization { base: Box::new(base), arguments }
                            }
                            _ => self.index(base)?,
                        }
                    } else if self.tokens.get(end).is_some_and(|token| token.kind == Kind::LParen) {
                        match crate::declaration::expression_type_arguments(
                            self.file, self.source, self.tokens, start,
                        ) {
                            Ok((arguments, end)) if matches!(self.tokens[start + 1].kind, Kind::Ident(_)) => {
                                self.cursor = end;
                                ExpressionKind::BracketedCallTarget { base: Box::new(base), arguments }
                            }
                            _ => self.index(base)?,
                        }
                    } else {
                        self.index(base)?
                    }
                }
                Kind::LBracket => self.index(base)?,
                Kind::LBrace => {
                    let fields = self.struct_fields()?;
                    ExpressionKind::Struct { target: Box::new(base), fields }
                }
                Kind::As => {
                    if matches!(self.current().kind, Kind::Ident(_)) {
                        let token = self.current().clone();
                        self.cursor += 1;
                        ExpressionKind::ScalarCast {
                            base: Box::new(base),
                            target: token.offset..token.end,
                        }
                    } else {
                        let (mutable, pointee) = self.pointer_cast_target()?;
                        ExpressionKind::PointerCast { base: Box::new(base), mutable, pointee }
                    }
                }
                Kind::Dot => {
                    let name = crate::parse_identifier(self.file, self.source, self.current())?.to_owned();
                    self.cursor += 1;
                    ExpressionKind::Field { base: Box::new(base), name }
                }
                Kind::ColonColon => {
                    let (name, end) = crate::pattern::parse_variant_suffix(
                        self.file, self.source, self.tokens, self.cursor - 1,
                    )?;
                    self.cursor = end;
                    ExpressionKind::Qualified { base: Box::new(base), name }
                }
                _ => unreachable!("postfix opener"),
            };
            base = Expression { kind, range: start..self.tokens[self.cursor - 1].end };
        }
        Ok(base)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_entry_leaves_statement_boundaries_to_the_caller() {
        let source = "let value = fetch()?; return value;";
        let tokens = crate::lex_diagnostic(source).unwrap();
        let (initializer, end) = parse_expression_tokens("case.ae", source, &tokens, 3, true).unwrap();
        assert!(matches!(initializer.kind, ExpressionKind::Propagate(_)));
        assert_eq!(&source[initializer.range], "fetch()?");
        assert_eq!(tokens[end].kind, Kind::Semi);
        let (_, end_without_propagation) =
            parse_expression_tokens("case.ae", source, &tokens, 3, false).unwrap();
        assert_eq!(tokens[end_without_propagation].kind, Kind::Question);
        let (value, end) = parse_expression_tokens("case.ae", source, &tokens, end + 2, false).unwrap();
        assert_eq!(&source[value.range], "value");
        assert_eq!(tokens[end].kind, Kind::Semi);
    }

    #[test]
    fn escaped_atoms_and_invalid_literals_preserve_source_ranges() {
        let source = r#" "a\n\"b" "#;
        let expression = parse_operator_expression("literal.ae", source).unwrap();
        assert_eq!(expression.range, 1..source.len() - 1);
        assert_eq!(expression.kind, ExpressionKind::Atom(Kind::Text(r#"a\n\"b"#.into())));

        let error = parse_operator_expression("literal.ae", r#" "a\n"#).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("E0003"));
        let span = error.primary.unwrap().span;
        assert_eq!(span.file.as_deref(), Some("literal.ae"));
        assert_eq!(span.range, 1..2);
    }

    #[test]
    fn arrays_and_vectors_keep_element_order_and_ranges() {
        let source = "[1 + 2, item]";
        let array = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Array(elements) = array.kind else {
            panic!("expected array");
        };
        assert_eq!(&source[elements[0].range.clone()], "1 + 2");
        assert_eq!(&source[elements[1].range.clone()], "item");

        let source = "vec[1.5, -2.5]";
        let vector = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(vector.range, 0..source.len());
        assert!(matches!(vector.kind, ExpressionKind::Vector(elements) if elements.len() == 2));
        assert!(matches!(parse_operator_expression("case.ae", "vec[]").unwrap().kind,
            ExpressionKind::Vector(elements) if elements.is_empty()));
        let error = parse_operator_expression("case.ae", "[]").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 1..2);
    }

    #[test]
    fn propagation_wraps_the_whole_statement_operand() {
        let source = "  maybe_find(flag)?";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(expression.range, 2..source.len());
        let ExpressionKind::Propagate(operand) = expression.kind else {
            panic!("expected a propagation marker");
        };
        assert!(matches!(operand.kind, ExpressionKind::Call { .. }));
        assert_eq!(&source[operand.range], "maybe_find(flag)");

        let error = parse_operator_expression("case.ae", "consume(maybe_find(flag)?)").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 24..25);
        assert!(parse_operator_expression("case.ae", "maybe_find(flag)??").is_err());
    }

    #[test]
    fn struct_initializers_preserve_field_order_and_nested_ranges() {
        let source = "Pair[i64, i64] { right: Point { x: 2 }, left: 1 + 3, }";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(expression.range, 0..source.len());
        let ExpressionKind::Struct { target, fields } = expression.kind else {
            panic!("expected struct literal");
        };
        assert!(matches!(target.kind, ExpressionKind::Specialization { .. }));
        assert_eq!(&source[fields[0].name_range.clone()], "right");
        assert_eq!(&source[fields[0].value.range.clone()], "Point { x: 2 }");
        assert_eq!(fields[1].name, "left");
        assert_eq!(&source[fields[1].value.range.clone()], "1 + 3");

        let error = parse_operator_expression("case.ae", "Point { x 2 }").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 10..11);
    }

    #[test]
    fn bracketed_call_targets_preserve_type_arguments_and_index_expressions() {
        let source = "make[Pair[i64, string], bool](7)";
        let parsed = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Call { callee, .. } = parsed.kind else {
            panic!("expected a call");
        };
        let ExpressionKind::BracketedCallTarget { base, arguments } = callee.kind else {
            panic!("expected bracketed callee");
        };
        assert_eq!(&source[base.range], "make");
        assert_eq!(arguments.into_iter().map(|range| &source[range]).collect::<Vec<_>>(),
            ["Pair[i64, string]", "bool"]);

        for source in ["items[0](7)", "items[i + 1](7)"] {
            let parsed = parse_operator_expression("case.ae", source).unwrap();
            assert!(matches!(parsed.kind, ExpressionKind::Call { callee, .. }
                if matches!(callee.kind, ExpressionKind::Index { .. })));
        }
        let parsed = parse_operator_expression("case.ae", "items[i](7)").unwrap();
        assert!(matches!(parsed.kind, ExpressionKind::Call { callee, .. }
            if matches!(callee.kind, ExpressionKind::BracketedCallTarget { .. })));
    }

    #[test]
    fn qualified_specializations_preserve_nested_type_arguments() {
        let source = "Outer[Inner[i64, string], bool]::Value";
        let parsed = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Qualified { base, .. } = parsed.kind else {
            panic!("expected qualified name");
        };
        let ExpressionKind::Specialization { arguments, .. } = base.kind else {
            panic!("expected specialization");
        };
        let arguments: Vec<_> = arguments.into_iter().map(|range| &source[range]).collect();
        assert_eq!(arguments, ["Inner[i64, string]", "bool"]);
        assert!(matches!(parse_operator_expression("case.ae", "items[i]").unwrap().kind,
            ExpressionKind::Index { .. }));
        assert!(parse_operator_expression("case.ae", "Outer[]::Value").is_err());
    }

    #[test]
    fn qualified_names_remain_distinct_from_field_access() {
        let source = "Result::Ok(value).field";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Field { base, name } = expression.kind else {
            panic!("expected field access");
        };
        assert_eq!(name, "field");
        let ExpressionKind::Call { callee, arguments } = base.kind else {
            panic!("expected constructor call");
        };
        assert_eq!(arguments.len(), 1);
        let ExpressionKind::Qualified { base, name } = callee.kind else {
            panic!("expected qualified name");
        };
        assert_eq!(name, "Ok");
        assert_eq!(&source[base.range], "Result");

        let error = parse_operator_expression("case.ae", "Result::(value)").unwrap_err();
        assert_eq!(error, crate::pattern::parse_variant_pattern("case.ae", "Result::(value)").unwrap_err());
        assert_eq!(error.primary.unwrap().span.range, 8..9);
    }

    #[test]
    fn address_casts_and_borrow_arguments_are_distinct() {
        let source = "call(&mut value as *mut i64, &other)";
        let parsed = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Call { arguments, .. } = parsed.kind else {
            panic!("expected a call");
        };
        let ExpressionKind::AddressCast { binding, mutable, pointee, .. } = &arguments[0].kind else {
            panic!("expected an address conversion");
        };
        assert_eq!(binding, "value");
        assert!(*mutable);
        assert_eq!(&source[pointee.clone()], "i64");
        assert!(matches!(arguments[1].kind, ExpressionKind::BorrowArgument { mutable: false, .. }));
        assert!(parse_operator_expression("case.ae", "&value as *mut i64").is_err());
    }

    #[test]
    fn pointer_cast_groups_before_method_call() {
        let source = "ptr as *mut Boxed[i64].deref()";
        let parsed = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Call { callee, .. } = parsed.kind else {
            panic!("expected method call");
        };
        let ExpressionKind::Field { base, name } = callee.kind else {
            panic!("expected method name");
        };
        assert_eq!(name, "deref");
        let ExpressionKind::PointerCast { mutable, pointee, .. } = base.kind else {
            panic!("expected pointer conversion");
        };
        assert!(mutable);
        assert_eq!(&source[pointee], "Boxed[i64]");
        assert_eq!(&source[base.range], "ptr as *mut Boxed[i64]");
        assert!(parse_operator_expression("case.ae", "ptr as *const Boxed[Boxed[i64]").is_err());
    }

    #[test]
    fn call_borrows_are_distinct_from_bitwise_and() {
        let source = "apply(&items, &mut point, x & y)";
        let parsed = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Call { arguments, .. } = parsed.kind else {
            panic!("expected call");
        };
        for (argument, expected, expected_mutability) in [
            (&arguments[0], "items", false),
            (&arguments[1], "point", true),
        ] {
            let ExpressionKind::BorrowArgument { mutable, name, name_range } = &argument.kind else {
                panic!("expected a borrowed argument");
            };
            assert_eq!(name, expected);
            assert_eq!(*mutable, expected_mutability);
            assert_eq!(&source[name_range.clone()], expected);
        }
        assert!(matches!(arguments[2].kind, ExpressionKind::Binary { operator: Kind::Amp, .. }));
        let error = parse_operator_expression("case.ae", "apply(&mut )").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 11..12);
    }

    #[test]
    fn postfix_binds_inside_prefix_and_before_multiplication() {
        let source = "-combine(values[0], point.x) * 3";
        let expression = parse_operator_expression("main.ae", source).unwrap();
        assert_eq!(expression.range, 0..source.len());
        let ExpressionKind::Binary { left, operator: Kind::Star, .. } = expression.kind else {
            panic!("expected multiplication");
        };
        let ExpressionKind::Prefix { operator: Kind::Minus, operand } = left.kind else {
            panic!("expected negation");
        };
        let ExpressionKind::Call { callee, arguments } = operand.kind else {
            panic!("expected call");
        };
        assert_eq!(callee.kind, ExpressionKind::Atom(Kind::Ident("combine".into())));
        assert_eq!(arguments.len(), 2);
        assert!(matches!(&arguments[0].kind, ExpressionKind::Index { .. }));
        assert!(matches!(&arguments[1].kind, ExpressionKind::Field { name, .. } if name == "x"));
        assert_eq!(&source[arguments[0].range.clone()], "values[0]");
        assert_eq!(&source[arguments[1].range.clone()], "point.x");

        let error = parse_operator_expression("main.ae", "combine(values[0)").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 16..17);
    }

    #[test]
    fn closure_literals_preserve_params_return_and_body_ranges() {
        let source = "|x: i64, y: string| -> i64 { return len(x); }";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(expression.range, 0..source.len());
        let ExpressionKind::Closure { params, return_type, body } = expression.kind else {
            panic!("expected a closure");
        };
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert_eq!(&source[params[0].name_range.clone()], "x");
        assert_eq!(&source[params[0].ty.clone()], "i64");
        assert_eq!(params[1].name, "y");
        assert_eq!(&source[params[1].ty.clone()], "string");
        assert_eq!(&source[return_type.clone()], "i64");
        assert_eq!(&source[body.clone()], "{ return len(x); }");

        // The empty-parameter shorthand, spaced as the run fixtures spell it.
        let source = "|| -> string { return \"hi\"; }";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(expression.range, 0..source.len());
        let ExpressionKind::Closure { params, return_type, body } = expression.kind else {
            panic!("expected a closure");
        };
        assert!(params.is_empty());
        assert_eq!(&source[return_type.clone()], "string");
        assert_eq!(&source[body.clone()], r#"{ return "hi"; }"#);

        // Function-type and borrowed parameter spellings ride the same
        // type-spelling capture as declarations.
        let source = "|f: fn(i64) -> i64, v: &vec[string]| -> i64 { return f(len(v)); }";
        let ExpressionKind::Closure { params, .. } =
            parse_operator_expression("case.ae", source).unwrap().kind
        else {
            panic!("expected a closure");
        };
        assert_eq!(&source[params[0].ty.clone()], "fn(i64) -> i64");
        assert_eq!(&source[params[1].ty.clone()], "&vec[string]");
    }

    #[test]
    fn closure_headers_mis_spell_like_the_compiler_rejects_them() {
        // Missing arrow: both parsers take(Kind::Arrow) at the brace.
        let error = parse_operator_expression("case.ae", "|x: i64| { return x; }").unwrap_err();
        assert_eq!(error.message, "expected Arrow, found LBrace");
        assert_eq!(error.code.as_deref(), Some("E0010"));
        // A trailing comma demands another binding, at the closing pipe.
        let error = parse_operator_expression("case.ae", "|x: i64,| -> i64 { return 1; }").unwrap_err();
        assert_eq!(error.message, "expected identifier");
        // A parameter without its type: the colon is required.
        let error = parse_operator_expression("case.ae", "|x| -> i64 { return 1; }").unwrap_err();
        assert_eq!(error.message, "expected Colon, found Pipe");
        // The 17th parameter trips the shared limit text.
        let params = (0..17).map(|i| format!("p{i}: i64")).collect::<Vec<_>>().join(", ");
        let error = parse_operator_expression("case.ae",
            &format!("|{params}| -> i64 {{ return 1; }}")).unwrap_err();
        assert!(error.message.contains("closure parameter limit exceeded (16)"));
        // An unterminated body points at the end of the stream.
        let error = parse_operator_expression("case.ae", "|| -> i64 { return 1;").unwrap_err();
        assert!(error.message.contains("expected '}' to close the closure body"));
    }

    #[test]
    fn self_forms_parse_as_plain_atoms() {
        // `Self` lexes as an identifier, so the impl forms ride the struct
        // literal and qualified-name paths without their own grammar.
        let source = "Self { x: 2 }";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        assert_eq!(expression.range, 0..source.len());
        let ExpressionKind::Struct { target, fields } = expression.kind else {
            panic!("expected a struct literal");
        };
        assert_eq!(target.kind, ExpressionKind::Atom(Kind::Ident("Self".into())));
        assert_eq!(fields[0].name, "x");

        let source = "Self::Value(3)";
        let expression = parse_operator_expression("case.ae", source).unwrap();
        let ExpressionKind::Call { callee, .. } = expression.kind else {
            panic!("expected a constructor call");
        };
        let ExpressionKind::Qualified { base, name } = callee.kind else {
            panic!("expected a qualified name");
        };
        assert_eq!(name, "Value");
        assert_eq!(base.kind, ExpressionKind::Atom(Kind::Ident("Self".into())));
    }

    #[test]
    fn malformed_and_deep_expressions_stop_at_a_source_boundary() {
        let error = parse_operator_expression("group.ae", "(2 * )").unwrap_err();
        assert_eq!(error.primary.unwrap().span.range, 5..6);

        let source = format!("{}2{}", "(".repeat(128), ")".repeat(128));
        let error = parse_operator_expression("group.ae", &source).unwrap_err();
        assert!(error.message.contains("complexity limit"));
        assert_eq!(error.primary.unwrap().span.range, 128..129);
    }
}

