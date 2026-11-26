use std::ops::Range;

use ae_span::Diagnostic;

use crate::{diagnostic_range, Kind, Token};

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternKind {
    Wildcard,
    Binding(String),
    /// `variant_range` is the variant name's byte span; semantic checks point
    /// at the variant (or the `(` that follows it) with this range.
    Variant {
        enum_name: String,
        variant: String,
        variant_range: Range<usize>,
        payload: Option<Vec<Pattern>>,
    },
}

/// Parse a qualified variant pattern and preserve payload order and source ranges.
pub fn parse_variant_pattern(file: &str, source: &str) -> Result<Pattern, Diagnostic> {
    let tokens = crate::lex_diagnostic(source).map_err(|mut diagnostic| {
        if let Some(label) = &mut diagnostic.primary {
            label.span.file = Some(file.to_owned());
        }
        diagnostic
    })?;
    let mut parser = Parser { file, source, tokens: &tokens, cursor: 0, nodes: 0 };
    let pattern = parser.pattern(true)?;
    if parser.current().kind != Kind::Eof {
        return Err(parser.error("expected end of pattern"));
    }
    Ok(pattern)
}

/// Read one qualified variant pattern from an EOF-terminated lexer stream,
/// returning the first unconsumed token. Match-arm callers continue from
/// there, so statement boundaries stay with the caller.
pub fn parse_pattern_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<(Pattern, usize), Diagnostic> {
    let mut parser = Parser { file, source, tokens, cursor: start, nodes: 0 };
    let pattern = parser.pattern(true)?;
    Ok((pattern, parser.cursor))
}

/// Read a variant name after the enum name, including its qualifier separator.
/// Returns the variant and the token index immediately after its name.
pub fn parse_variant_suffix(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<(String, usize), Diagnostic> {
    let separator = &tokens[start];
    if separator.kind != Kind::ColonColon {
        return Err(diagnostic_range(file, source, separator.offset..separator.end,
            &format!("expected ColonColon, found {:?}", separator.kind)).with_code("E0010"));
    }
    let name = crate::parse_identifier(file, source, &tokens[start + 1])?;
    Ok((name.to_owned(), start + 2))
}

struct Parser<'a> {
    file: &'a str,
    source: &'a str,
    tokens: &'a [Token],
    cursor: usize,
    nodes: usize,
}

impl Parser<'_> {
    fn current(&self) -> &Token {
        &self.tokens[self.cursor]
    }

    fn error(&self, message: &str) -> Diagnostic {
        let token = self.current();
        diagnostic_range(self.file, self.source, token.offset..token.end, message)
    }

    fn consume(&mut self, kind: Kind) -> bool {
        if self.current().kind != kind {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn pattern(&mut self, qualified: bool) -> Result<Pattern, Diagnostic> {
        if self.nodes == 128 {
            return Err(self.error("pattern complexity limit exceeded (128)"));
        }
        self.nodes += 1;
        let start = self.current().offset;
        let name = crate::parse_identifier(self.file, self.source, self.current())?.to_owned();
        self.cursor += 1;
        let kind = if name == "_" && self.current().kind != Kind::ColonColon {
            PatternKind::Wildcard
        } else if qualified || self.current().kind == Kind::ColonColon {
            let (variant, end) = parse_variant_suffix(self.file, self.source, self.tokens, self.cursor)?;
            let name_token = &self.tokens[self.cursor + 1];
            let variant_range = name_token.offset..name_token.end;
            self.cursor = end;
            // Empty parentheses parse as an empty payload list; whether a
            // variant may carry payloads is a semantic question, so the
            // compiler rejects the shape against the declaration.
            let payload = if self.consume(Kind::LParen) {
                if self.consume(Kind::RParen) {
                    Some(Vec::new())
                } else {
                    let mut fields = vec![self.pattern(false)?];
                    while self.consume(Kind::Comma) {
                        fields.push(self.pattern(false)?);
                    }
                    if !self.consume(Kind::RParen) {
                        return Err(self.error("expected ',' or ')' after payload pattern"));
                    }
                    Some(fields)
                }
            } else {
                None
            };
            PatternKind::Variant { enum_name: name, variant, variant_range, payload }
        } else {
            PatternKind::Binding(name)
        };
        Ok(Pattern { kind, range: start..self.tokens[self.cursor - 1].end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_variant_payloads_preserve_binding_order_and_ranges() {
        let source = "Outer::Item(first, Inner::Pair(second, third), last)";
        let tree = parse_variant_pattern("case.ae", source).unwrap();
        assert_eq!(tree.range, 0..source.len());
        let PatternKind::Variant { payload: Some(fields), .. } = tree.kind else {
            panic!("expected outer payload");
        };
        let PatternKind::Variant { payload: Some(inner), .. } = &fields[1].kind else {
            panic!("expected nested payload");
        };
        for (field, name) in [
            (&fields[0], "first"), (&inner[0], "second"), (&inner[1], "third"), (&fields[2], "last"),
        ] {
            assert_eq!(field.kind, PatternKind::Binding(name.into()));
            assert_eq!(&source[field.range.clone()], name);
        }
    }

    #[test]
    fn missing_nested_binding_points_at_the_closing_parenthesis() {
        let source = "Outer::Item(Inner::Pair(first, ))";
        let error = parse_variant_pattern("case.ae", source).unwrap_err();
        let span = error.primary.unwrap().span;
        let start = source.find(')').unwrap();
        assert_eq!(span.range, start..start + 1);
    }

    #[test]
    fn token_entry_stops_before_the_arm_arrow() {
        let source = "Option::Some(value) => { print(value); }";
        let tokens = crate::lex_diagnostic(source).unwrap();
        let (tree, end) = parse_pattern_tokens("case.ae", source, &tokens, 0).unwrap();
        assert_eq!(&source[tree.range], "Option::Some(value)");
        assert_eq!(tokens[end].kind, Kind::FatArrow);

        let source = "Pair::Value(first, second) => {}";
        let tokens = crate::lex_diagnostic(source).unwrap();
        let (tree, end) = parse_pattern_tokens("case.ae", source, &tokens, 0).unwrap();
        let PatternKind::Variant { variant, payload: Some(fields), .. } = tree.kind else {
            panic!("expected a variant payload");
        };
        assert_eq!(variant, "Value");
        assert_eq!(fields.iter().map(|f| &source[f.range.clone()]).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(tokens[end].kind, Kind::FatArrow);
    }

    #[test]
    fn empty_parentheses_parse_as_an_empty_payload() {
        // The grammar accepts `()`; the compiler decides against the
        // declaration whether the variant may carry payloads at all.
        let source = "Option::None()";
        let tree = parse_variant_pattern("case.ae", source).unwrap();
        let PatternKind::Variant { variant, variant_range, payload: Some(fields), .. } = tree.kind
        else {
            panic!("expected an empty payload list");
        };
        assert_eq!(variant, "None");
        assert!(fields.is_empty());
        let start = source.find("None").unwrap();
        assert_eq!(variant_range, start..start + 4);
    }
}
