use std::ops::Range;

use crate::{Kind, Token};

#[derive(Debug, PartialEq)]
pub enum LiteralValue<'a> {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Char(char),
    /// Source spelling between the quotes, with escape sequences preserved.
    String(&'a str),
}

#[derive(Debug, PartialEq)]
pub struct Literal<'a> {
    pub value: LiteralValue<'a>,
    pub range: Range<usize>,
    pub line: usize,
    pub column: usize,
}

/// Parse one literal atom. Prefix operators and postfix expressions belong
/// to the enclosing expression parser.
pub fn parse_literal(token: &Token) -> Option<Literal<'_>> {
    let value = match &token.kind {
        Kind::Number(value) => LiteralValue::Integer(*value),
        Kind::Float(value) => LiteralValue::Float(*value),
        Kind::True => LiteralValue::Boolean(true),
        Kind::False => LiteralValue::Boolean(false),
        Kind::Char(value) => LiteralValue::Char(*value),
        Kind::Text(value) => LiteralValue::String(value),
        _ => return None,
    };
    Some(Literal {
        value,
        range: token.offset..token.end,
        line: token.line,
        column: token.column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_preserve_values_and_token_boundaries() {
        let source = r#"255 1.5 true false "a\n" identifier"#;
        let tokens = crate::lex_diagnostic(source).unwrap();
        for (token, (value, spelling)) in tokens.iter().zip([
            (LiteralValue::Integer(255), "255"),
            (LiteralValue::Float(1.5), "1.5"),
            (LiteralValue::Boolean(true), "true"),
            (LiteralValue::Boolean(false), "false"),
            (LiteralValue::String(r"a\n"), r#""a\n""#),
        ]) {
            let literal = parse_literal(token).unwrap();
            assert_eq!(literal.value, value);
            assert_eq!(&source[literal.range], spelling);
        }
        assert!(parse_literal(&tokens[5]).is_none());
    }
}
