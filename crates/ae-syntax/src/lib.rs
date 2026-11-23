//! Lexer: source text to tokens, plus location helpers.
#![allow(clippy::result_large_err)]
pub mod declaration;
pub mod expression;
pub mod fmt;
pub mod literal;
pub mod module;
pub mod operator;
pub mod pattern;
pub mod statement;
use ae_span::{Diagnostic, SourceFile};
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Ident(String), Number(i64), Float(f64), Text(String), Fn, Let, Var, Mut, Print, If, Else, While, For, In, Return, Break, Continue, Assert, True, False, Use,
    EqEq, NotEq, Less, LessEq, Greater, GreaterEq, And, Or, Bang, Pipe,
    LParen, RParen, LBrace, RBrace, LBracket, RBracket, Comma, Colon, Arrow, Amp, Semi, Equal, Plus, Minus, Star, Slash, Percent, Caret, Shl, Shr, PlusEqual, MinusEqual, StarEqual, SlashEqual, PercentEqual, Dot, DotDot, Struct,
    Enum, Match, ColonColon, FatArrow, Question, Trait, Impl, Unsafe, Const, As, Extern, Hash, Eof,
}
#[derive(Debug, Clone)]
pub struct Token { pub kind: Kind, pub offset: usize, pub end: usize, pub line: usize, pub column: usize }

// Test-only view for the formatter's lexer-drift defense: the same token
// boundaries, as comparable names. Values are dropped on purpose.
pub fn lex_kind_names(source: &str) -> Result<Vec<&'static str>, String> {
    lex(source).map(|tokens| tokens.iter().map(|t| kind_name(&t.kind)).collect())
}

fn kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Ident(_) => "Ident", Kind::Number(_) => "Number", Kind::Float(_) => "Float", Kind::Text(_) => "Text",
        Kind::Fn => "Fn", Kind::Let => "Let", Kind::Var => "Var", Kind::Mut => "Mut", Kind::Print => "Print",
        Kind::If => "If", Kind::Else => "Else", Kind::While => "While", Kind::For => "For",
        Kind::In => "In", Kind::Return => "Return", Kind::Break => "Break", Kind::Continue => "Continue",
        Kind::Assert => "Assert", Kind::True => "True", Kind::False => "False", Kind::Use => "Use",
        Kind::Struct => "Struct", Kind::Enum => "Enum", Kind::Match => "Match",
        Kind::EqEq => "EqEq", Kind::NotEq => "NotEq", Kind::Less => "Less", Kind::LessEq => "LessEq",
        Kind::Greater => "Greater", Kind::GreaterEq => "GreaterEq", Kind::And => "And", Kind::Or => "Or",
        Kind::Bang => "Bang", Kind::Pipe => "Pipe",
        Kind::LParen => "LParen", Kind::RParen => "RParen", Kind::LBrace => "LBrace", Kind::RBrace => "RBrace",
        Kind::LBracket => "LBracket", Kind::RBracket => "RBracket", Kind::Comma => "Comma",
        Kind::Colon => "Colon", Kind::Arrow => "Arrow", Kind::Amp => "Amp", Kind::Semi => "Semi",
        Kind::Equal => "Equal", Kind::Plus => "Plus", Kind::Minus => "Minus", Kind::Star => "Star",
        Kind::PlusEqual => "PlusEqual", Kind::MinusEqual => "MinusEqual", Kind::StarEqual => "StarEqual", Kind::SlashEqual => "SlashEqual", Kind::PercentEqual => "PercentEqual", Kind::Caret => "Caret", Kind::Shl => "Shl", Kind::Shr => "Shr",
        Kind::Slash => "Slash", Kind::Percent => "Percent", Kind::Dot => "Dot", Kind::DotDot => "DotDot",
        Kind::ColonColon => "ColonColon", Kind::FatArrow => "FatArrow", Kind::Question => "Question",
        Kind::Trait => "Trait", Kind::Impl => "Impl", Kind::Unsafe => "Unsafe", Kind::Const => "Const", Kind::As => "As", Kind::Extern => "Extern", Kind::Hash => "Hash", Kind::Eof => "Eof",
    }
}

fn clamp_char_boundary(source: &str, offset: usize) -> usize {
    let mut idx = offset.min(source.len());
    while idx > 0 && !source.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

// Lexer diagnostics carry no file name; load_file adds the file prefix.
fn lex_error(source: &str, offset: usize, code: &str, message: &str) -> Diagnostic {
    let clamped = clamp_char_boundary(source, offset);
    let end = clamped + source[clamped..].chars().next().map_or(0, char::len_utf8);
    let mut diagnostic = match (SourceFile { name: None, text: source }).span(clamped..end) {
        Some(span) => Diagnostic::at(span, message),
        None => Diagnostic {
            code: None,
            message: message.to_owned(),
            primary: None,
            secondary: Vec::new(),
            suggestions: Vec::new(),
        },
    };
    diagnostic.code = Some(code.to_owned());
    diagnostic
}
pub fn error(file: &str, source: &str, offset: usize, message: &str) -> String {
    diagnostic(file, source, offset, message).to_string()
}
pub fn diagnostic(file: &str, source: &str, offset: usize, message: &str) -> Diagnostic {
    diagnostic_range(file, source, offset..offset, message)
}
pub fn diagnostic_range(file: &str, source: &str, range: std::ops::Range<usize>, message: &str) -> Diagnostic {
    let start = clamp_char_boundary(source, range.start);
    let end = clamp_char_boundary(source, range.end).max(start);
    match (SourceFile { name: Some(file), text: source }).span(start..end) {
        Some(span) => Diagnostic::at(span, message),
        None => Diagnostic {
            code: None,
            message: message.to_owned(),
            primary: None,
            secondary: Vec::new(),
            suggestions: Vec::new(),
        },
    }
}
/// Whether a doc comment (`///` line) sits in the comment block immediately
/// above `offset`. The parser uses this for the E0704 precondition rule
/// (spec/unsafe.md 6): every `unsafe fn` carries a doc comment naming what
/// the caller must guarantee. Comments themselves are skipped by the lexer,
/// so the check scans the source text backwards over the contiguous comment
/// lines and blanks; the first non-comment line ends the walk. A plain
/// `//` line inside the block does not break it (a doc block commonly mixes
/// prose lines), but the block must contain at least one `///` line.
pub fn has_doc_comment_above(source: &str, offset: usize) -> bool {
    let mut end = clamp_char_boundary(source, offset);
    // Walk back over the whitespace between the declaration and its comment
    // block; a token ends the walk only if it is not whitespace (the caller
    // passes the declaration keyword's offset, so line-leading whitespace
    // separates the two).
    while let Some(c) = source[..end].chars().next_back() {
        if c.is_whitespace() { end -= c.len_utf8(); } else { break; }
    }
    // `end` is now just past the last character of the preceding line (or 0).
    let mut found = false;
    loop {
        let line_start = source[..end].rfind('\n').map(|n| n + 1).unwrap_or(0);
        let line = source[line_start..end].trim_start();
        if line.starts_with("///") { found = true; }
        else if line.starts_with("//") { /* prose line inside the block */ }
        else if !line.is_empty() { break; }
        if line_start == 0 { break; }
        end = line_start - 1;
    }
    found
}

pub fn lex(source: &str) -> Result<Vec<Token>, String> {
    lex_diagnostic(source).map_err(|error| error.to_string())
}

enum LexFailure { Unexpected(Diagnostic), Fatal(Diagnostic) }
impl From<Diagnostic> for LexFailure {
    fn from(diagnostic: Diagnostic) -> Self { Self::Fatal(diagnostic) }
}

const MAX_LEX_DIAGNOSTICS: usize = 32;

/// Recover from unexpected characters, closed strings with unsupported escapes,
/// and complete invalid numeric literals,
/// never returning tokens for an invalid source. All positions are calculated against the original input.
pub fn lex_with_diagnostics(source: &str) -> Result<Vec<Token>, Vec<Diagnostic>> {
    match lex_recovered(source) {
        LexOutcome::Clean(tokens) => Ok(tokens),
        LexOutcome::Recovered(_, diagnostics) | LexOutcome::Fatal(diagnostics) => Err(diagnostics),
    }
}

// The full picture of a lexical pass: the token stream it produced, the
// diagnostics it raised, and whether the stream is trustworthy. `Clean` is
// the only all-clear outcome; `Recovered` means the stream is complete but
// some characters were skipped or repaired; `Fatal` means the stream is
// partial and callers must not parse it.
pub enum LexOutcome {
    Clean(Vec<Token>),
    Recovered(Vec<Token>, Vec<Diagnostic>),
    Fatal(Vec<Diagnostic>),
}

pub fn lex_recovered(source: &str) -> LexOutcome {
    let mut diagnostics = Vec::new();
    match lex_inner(source, Some(&mut diagnostics)) {
        Ok(tokens) if diagnostics.is_empty() => LexOutcome::Clean(tokens),
        Ok(tokens) => LexOutcome::Recovered(tokens, diagnostics),
        Err(LexFailure::Fatal(diagnostic)) => {
            diagnostics.push(diagnostic);
            LexOutcome::Fatal(diagnostics)
        }
        Err(LexFailure::Unexpected(diagnostic)) => {
            // With a recovery sink, unexpected characters are repaired, so
            // this arm means a caller reached lex_inner without one.
            diagnostics.push(diagnostic);
            LexOutcome::Fatal(diagnostics)
        }
    }
}
/// Lex with a structured diagnostic, preserving byte offset and location.
pub fn lex_diagnostic(source: &str) -> Result<Vec<Token>, Diagnostic> {
    lex_inner(source, None).map_err(|failure| match failure {
        LexFailure::Unexpected(diagnostic) | LexFailure::Fatal(diagnostic) => diagnostic,
    })
}
// Determine whether the rest of a malformed string has an unambiguous end.
// A backslash consumes the next Unicode scalar even when that escape is invalid.
fn has_closing_quote(source: &str, mut offset: usize) -> bool {
    while let Some(ch) = source.get(offset..).and_then(|s| s.chars().next()) {
        match ch {
            '"' => return true,
            '\n' | '\0' => return false,
            '\\' => {
                offset += 1;
                let Some(next) = source.get(offset..).and_then(|s| s.chars().next()) else { return false };
                if next == '\n' || next == '\0' { return false; }
                offset += next.len_utf8();
            }
            _ => offset += ch.len_utf8(),
        }
    }
    false
}

fn lex_inner(source: &str, mut recovered: Option<&mut Vec<Diagnostic>>) -> Result<Vec<Token>, LexFailure> {
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut result = Vec::new();
    let (mut location_offset, mut line, mut column) = (0, 1, 1);
    macro_rules! recover_numeric {
        ($diagnostic:expr) => {{
            let diagnostic = $diagnostic;
            match recovered.as_deref_mut() {
                Some(diagnostics) => {
                    diagnostics.push(diagnostic);
                    if diagnostics.len() >= MAX_LEX_DIAGNOSTICS { return Ok(result); }
                    continue;
                }
                None => return Err(LexFailure::Fatal(diagnostic)),
            }
        }};
    }
    while let Some(c) = source[i..].chars().next() {
        let start = i;
        if c.is_whitespace() { i += c.len_utf8(); continue; }
        if bytes[i..].starts_with(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        let kind = if c == '"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => break,
                    b'\n' | b'\0' => return Err(lex_error(source, i, "E0004", "string literals must not contain raw newlines or NUL; use escapes").into()),
                    b'\\' => {
                        let escaped = bytes.get(i + 1).copied();
                        if !matches!(escaped, Some(b'n' | b't' | b'r' | b'\\' | b'"')) {
                            let diagnostic = lex_error(source, i, "E0002", "unsupported string escape; allowed: \\n \\t \\r \\\\ \\\""
                            ).with_suggestion("Use a supported escape, or escape a literal backslash as \\\\.");
                            let Some(diagnostics) = recovered.as_deref_mut().filter(|_| has_closing_quote(source, i)) else {
                                return Err(diagnostic.into());
                            };
                            diagnostics.push(diagnostic);
                            if diagnostics.len() >= MAX_LEX_DIAGNOSTICS { return Ok(result); }
                            i += 1;
                            if let Some(next) = source.get(i..).and_then(|s| s.chars().next()) {
                                i += next.len_utf8();
                            }
                            continue;
                        }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            if i >= bytes.len() { return Err(lex_error(source, start, "E0003", "unterminated string literal").into()); }
            i += 1;
            Kind::Text(source[start + 1..i - 1].to_owned())
        } else if c.is_ascii_alphabetic() || c == '_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            match &source[start..i] {
                "fn" => Kind::Fn, "let" => Kind::Let, "var" => Kind::Var, "mut" => Kind::Mut, "print" => Kind::Print,
                "if" => Kind::If, "else" => Kind::Else, "while" => Kind::While, "return" => Kind::Return,
                "for" => Kind::For, "in" => Kind::In,
                "break" => Kind::Break, "continue" => Kind::Continue, "assert" => Kind::Assert,
                "true" => Kind::True, "false" => Kind::False, "use" => Kind::Use, "struct" => Kind::Struct,
                "enum" => Kind::Enum, "match" => Kind::Match, "trait" => Kind::Trait, "impl" => Kind::Impl, "unsafe" => Kind::Unsafe, "const" => Kind::Const, "as" => Kind::As, "extern" => Kind::Extern,
                s => Kind::Ident(s.to_owned()),
            }
        } else if c.is_ascii_digit() {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            // A fraction needs a digit after the dot: `1..2` is a range and
            // `1.field` would be a field access, so neither may become a float.
            let mut floating = false;
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                floating = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            }
            // An exponent also makes the literal a float: `1e3` is 1000.0.
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut exponent = i + 1;
                if exponent < bytes.len() && (bytes[exponent] == b'+' || bytes[exponent] == b'-') { exponent += 1; }
                if exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
                    floating = true;
                    i = exponent;
                    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                }
            }
            let text = &source[start..i];
            if floating {
                let value: f64 = match text.parse() {
                    Ok(value) => value,
                    Err(_) => recover_numeric!(lex_error(source, start, "E0006", "invalid float literal")),
                };
                if !value.is_finite() { recover_numeric!(lex_error(source, start, "E0007", "float literal is not finite")); }
                // Rounding a nonzero literal down to zero would silently replace
                // the value the user wrote, so it is rejected instead of stored.
                let mantissa = text.split(['e', 'E']).next().unwrap_or(text);
                if value == 0.0 && mantissa.bytes().any(|b| b.is_ascii_digit() && b != b'0') {
                    recover_numeric!(lex_error(source, start, "E0008", "float literal underflows to zero"));
                }
                Kind::Float(value)
            } else {
                match text.parse() {
                    Ok(value) => Kind::Number(value),
                    Err(_) => recover_numeric!(lex_error(source, start, "E0005", "integer literal exceeds i64 maximum")),
                }
            }
        } else if ["==", "!=", "<=", ">=", "->", "&&", "||", "..", "=>", "::"].iter().any(|op| source[i..].starts_with(op)) {
            i += 2;
            match &source[start..i] { "==" => Kind::EqEq, "!=" => Kind::NotEq, "<=" => Kind::LessEq, ">=" => Kind::GreaterEq, "->" => Kind::Arrow, "&&" => Kind::And, "||" => Kind::Or, ".." => Kind::DotDot, "=>" => Kind::FatArrow, "::" => Kind::ColonColon, _ => unreachable!() }
        } else {
            i += c.len_utf8();
            match c {
                '(' => Kind::LParen, ')' => Kind::RParen, '{' => Kind::LBrace, '}' => Kind::RBrace,
                '[' => Kind::LBracket, ']' => Kind::RBracket, ',' => Kind::Comma, ':' => Kind::Colon, '&' => Kind::Amp,
                ';' => Kind::Semi, '=' => Kind::Equal, '+' => if bytes.get(i) == Some(&b'=') { i += 1; Kind::PlusEqual } else { Kind::Plus }, '-' => if bytes.get(i) == Some(&b'=') { i += 1; Kind::MinusEqual } else { Kind::Minus },
                '*' => if bytes.get(i) == Some(&b'=') { i += 1; Kind::StarEqual } else { Kind::Star }, '/' => if bytes.get(i) == Some(&b'=') { i += 1; Kind::SlashEqual } else { Kind::Slash }, '%' => if bytes.get(i) == Some(&b'=') { i += 1; Kind::PercentEqual } else { Kind::Percent }, '<' => if bytes.get(i) == Some(&b'<') { i += 1; Kind::Shl } else { Kind::Less }, '>' => if bytes.get(i) == Some(&b'>') { i += 1; Kind::Shr } else { Kind::Greater }, '^' => Kind::Caret,
                '!' => Kind::Bang, '.' => Kind::Dot, '?' => Kind::Question, '#' => Kind::Hash,
                '|' => Kind::Pipe,
                _ => {
                    let diagnostic = lex_error(source, start, "E0001", &format!("unexpected character {c:?}"));
                    match recovered.as_deref_mut() {
                        Some(diagnostics) => {
                            diagnostics.push(diagnostic);
                            if diagnostics.len() >= MAX_LEX_DIAGNOSTICS { return Ok(result); }
                            continue;
                        }
                        None => return Err(LexFailure::Unexpected(diagnostic)),
                    }
                }
            }
        };
        for character in source[location_offset..start].chars() {
            if character == '\n' { line += 1; column = 1; } else { column += 1; }
        }
        location_offset = start;
        result.push(Token { kind, offset: start, end: i, line, column });
    }
    for character in source[location_offset..].chars() {
        if character == '\n' { line += 1; column = 1; } else { column += 1; }
    }
    result.push(Token { kind: Kind::Eof, offset: source.len(), end: source.len(), line, column });
    Ok(result)
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn lexical_error_codes_identify_distinct_failure_categories() {
        for (source, code) in [
            ("@", "E0001"),
            ("\"bad\\q\"", "E0002"),
            ("\"unterminated", "E0003"),
            ("\"line\nbreak\"", "E0004"),
            ("9223372036854775808", "E0005"),
            ("1e999", "E0007"),
            ("1e-999", "E0008"),
        ] {
            let diagnostic = lex_diagnostic(source).unwrap_err();
            assert_eq!(diagnostic.code.as_deref(), Some(code), "{source:?}");
        }
    }

    #[test]
    fn token_ranges_cover_original_utf8_bytes_and_empty_eof() {
        let source = "// 中文\nlet word = \"😀\";";
        let tokens = lex(source).unwrap();
        let spellings: Vec<_> = tokens.iter().map(|token| &source[token.offset..token.end]).collect();
        assert_eq!(spellings, ["let", "word", "=", "\"😀\"", ";", ""]);
        let eof = tokens.last().unwrap();
        assert_eq!((eof.offset, eof.end), (source.len(), source.len()));
    }

    #[test]
    fn strict_and_recovering_lexers_agree_on_string_literal_boundaries() {
        let prefix = "// \u{00e9}\n";
        let literal = r#""é\n\t\r\\\"""#;
        let source = format!("{prefix}{literal};");
        let strict = lex_diagnostic(&source).unwrap();
        let LexOutcome::Clean(recovered) = lex_recovered(&source) else {
            panic!("valid string did not lex cleanly");
        };
        assert_eq!(strict.len(), recovered.len());
        for (left, right) in strict.iter().zip(&recovered) {
            assert_eq!(left.kind, right.kind);
            assert_eq!((left.offset, left.end, left.line, left.column),
                       (right.offset, right.end, right.line, right.column));
        }
        let token = &strict[0];
        assert_eq!((token.offset, token.end), (prefix.len(), prefix.len() + literal.len()));
        assert_eq!(token.kind, Kind::Text(literal[1..literal.len() - 1].to_owned()));

        let unterminated = format!("{prefix}\"\\n");
        let strict = lex_diagnostic(&unterminated).unwrap_err();
        let LexOutcome::Fatal(recovered) = lex_recovered(&unterminated) else {
            panic!("unterminated string did not stop lexing");
        };
        assert_eq!(recovered, vec![strict.clone()]);
        assert_eq!(strict.code.as_deref(), Some("E0003"));
        assert_eq!(strict.primary.unwrap().span.range, prefix.len()..prefix.len() + 1);
    }

    #[test]
    fn lexical_errors_keep_byte_offset_and_unicode_column() {
        let source = "// 中文\n  @";
        let diagnostic = lex_diagnostic(source).unwrap_err();
        let span = &diagnostic.primary.as_ref().unwrap().span;
        assert_eq!(span.range, source.find('@').unwrap()..source.find('@').unwrap() + 1);
        assert_eq!((span.line, span.column), (2, 3));
        assert_eq!(diagnostic.to_string(), lex(source).unwrap_err());
    }

    #[test]
    fn lexical_error_spans_cover_unicode_character_and_unterminated_quote() {
        let diagnostic = lex_diagnostic("中").unwrap_err();
        assert_eq!(diagnostic.primary.unwrap().span.range, 0.."中".len());
        let diagnostic = lex_diagnostic("\"unterminated").unwrap_err();
        assert_eq!(diagnostic.primary.unwrap().span.range, 0..1);
    }

    #[test]
    fn recovery_continues_after_complete_invalid_numeric_literals() {
        let source = "// 中文\n9223372036854775808 @ 1e999 1e-999";
        let errors = lex_with_diagnostics(source).unwrap_err();
        let codes: Vec<_> = errors.iter().map(|d| d.code.as_deref().unwrap()).collect();
        assert_eq!(codes, ["E0005", "E0001", "E0007", "E0008"]);
        for (error, needle) in errors.iter().zip(["9223372036854775808", "@", "1e999", "1e-999"]) {
            let span = &error.primary.as_ref().unwrap().span;
            assert_eq!(span.range.start, source.find(needle).unwrap());
            assert_eq!(span.line, 2);
        }
        assert_eq!(lex_diagnostic(source).unwrap_err(), errors[0]);
        let many = std::iter::repeat_n("9223372036854775808", 40).collect::<Vec<_>>().join(" ");
        assert_eq!(lex_with_diagnostics(&many).unwrap_err().len(), 32);
        assert!(lex_with_diagnostics("9223372036854775808 \"bad\\q\" @").unwrap_err().len() == 3);
    }

    #[test]
    fn recovery_handles_closed_strings_with_bad_escapes() {
        let source = r#""bad\q \中 \" @ still string \z" @"#;
        let errors = lex_with_diagnostics(source).unwrap_err();
        let codes: Vec<_> = errors.iter().map(|d| d.code.as_deref().unwrap()).collect();
        assert_eq!(codes, ["E0002", "E0002", "E0002", "E0001"]);
        assert_eq!(lex_diagnostic(source).unwrap_err(), errors[0]);
        assert_eq!(errors[3].primary.as_ref().unwrap().span.range.start, source.rfind('@').unwrap());
        let many = format!("\"{}\" @", "\\q".repeat(40));
        assert_eq!(lex_with_diagnostics(&many).unwrap_err().len(), 32);
        for source in ["\"bad\\q", "\"bad\\q\n@", "\"bad\\q\0@"] {
            let errors = lex_with_diagnostics(source).unwrap_err();
            assert_eq!(errors.len(), 1, "{source:?}");
            assert_eq!(errors[0].code.as_deref(), Some("E0002"));
        }
    }

    #[test]
    fn recovery_keeps_original_positions_and_stops_at_ambiguous_errors() {
        let errors = lex_with_diagnostics("中 @\n\"bad\\q\"").unwrap_err();
        assert_eq!(errors.len(), 3);
        let positions: Vec<_> = errors.iter().map(|d| {
            let span = &d.primary.as_ref().unwrap().span;
            (span.range.clone(), span.line, span.column)
        }).collect();
        assert_eq!(positions, vec![(0..3, 1, 1), (4..5, 1, 3), (10..11, 2, 5)]);
        assert!(errors[2].message.contains("unsupported string escape"));
        assert!(lex_with_diagnostics("fn main() {} // @").is_ok());
        assert_eq!(lex_with_diagnostics(&"@".repeat(40)).unwrap_err().len(), 32);
    }
}

/// A contiguous `///` block and the declaration it sits above.
#[derive(Debug)]
pub struct DocBlock {
    /// One-based line of the block's first `///`.
    pub line: usize,
    /// Markdown body: the `///` markers and one following space removed.
    pub text: String,
    /// First line of the next non-blank, non-doc line, trimmed; empty when the
    /// block is not followed by a declaration.
    pub declaration: String,
}

/// Collects doc-comment blocks in source order.
pub fn doc_blocks(source: &str) -> Vec<DocBlock> {
    let lines: Vec<&str> = source.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if trimmed.strip_prefix("///").is_none() {
            index += 1;
            continue;
        }
        let start = index;
        let mut text = String::new();
        while index < lines.len() {
            let line = lines[index].trim_start();
            let Some(doc) = line.strip_prefix("///") else { break; };
            text.push_str(doc.strip_prefix(' ').unwrap_or(doc));
            text.push('\n');
            index += 1;
        }
        let mut lookahead = index;
        while lookahead < lines.len() && lines[lookahead].trim().is_empty() { lookahead += 1; }
        let declaration = lines.get(lookahead).map(|line| line.trim()).unwrap_or("")
            .split(['{', ';']).next().unwrap_or("").trim().to_owned();
        blocks.push(DocBlock { line: start + 1, text, declaration });
    }
    blocks
}

/// One fenced example taken from a doc comment.
#[derive(Debug)]
pub struct DocExample {
    /// One-based line of the opening fence.
    pub line: usize,
    /// The fence body, newline-terminated.
    pub source: String,
}

/// Collects fenced code blocks from doc comments. A fence opens on a `///` line
/// whose text begins with ``` and closes on a `///` line that is only ```;
/// anything else while open is body. An unclosed fence is an error, not a silent
/// skip, because a doc test that quietly disappears is worse than a failing one.
pub fn doc_examples(source: &str) -> Result<Vec<DocExample>, String> {
    let mut examples = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (index, line) in source.lines().enumerate() {
        let Some(doc) = line.trim_start().strip_prefix("///") else {
            if let Some((line, _)) = open {
                return Err(format!("line {line}: doc-comment code fence is never closed"));
            }
            continue;
        };
        let text = doc.strip_prefix(' ').unwrap_or(doc);
        let marker = text.trim();
        match open.as_mut() {
            Some((_, body)) => {
                if marker == "```" {
                    let (line, source) = open.take().unwrap();
                    examples.push(DocExample { line, source });
                } else {
                    body.push_str(text);
                    body.push('\n');
                }
            }
            None => {
                if marker == "```" {
                    return Err(format!("line {}: doc-comment code fence closes without opening", index + 1));
                }
                if marker.starts_with("```") {
                    open = Some((index + 1, String::new()));
                }
            }
        }
    }
    if let Some((line, _)) = open {
        return Err(format!("line {line}: doc-comment code fence is never closed"));
    }
    Ok(examples)
}


#[cfg(test)]
mod doc_tests {
    use super::*;

    #[test]
    fn collects_doc_blocks_with_their_declaration() {
        let source = "/// Adds.\n///\n/// ```ae\n/// fn main() { print(1); }\n/// ```\nfn add(a: i64) -> i64 { return a; }\n";
        let blocks = doc_blocks(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].line, 1);
        assert!(blocks[0].text.contains("Adds."));
        assert_eq!(blocks[0].declaration, "fn add(a: i64) -> i64");
        let examples = doc_examples(source).unwrap();
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].line, 3);
        assert_eq!(examples[0].source, "fn main() { print(1); }\n");
    }

    #[test]
    fn unclosed_or_stray_fences_are_reported_with_their_line() {
        assert_eq!(doc_examples("/// ```ae\n/// fn main() {}\nfn main() {}\n").unwrap_err(),
            "line 1: doc-comment code fence is never closed");
        assert_eq!(doc_examples("/// ```\n").unwrap_err(),
            "line 1: doc-comment code fence closes without opening");
        assert!(doc_examples("// ```ae\n// not doc\nfn main() {}\n").unwrap().is_empty());
    }

    #[test]
    fn blocks_stop_at_the_first_non_doc_line() {
        let source = "/// one\nfn a() {}\n/// two\n\nfn b() {}\n";
        let blocks = doc_blocks(source);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].declaration, "fn a()");
        assert_eq!(blocks[1].declaration, "fn b()");
    }
}


/// Read a nonempty comma-separated list. The caller handles the enclosing
/// delimiters and supplies the parser and diagnostics for each item.
pub fn parse_comma_list<P, T, E>(
    parser: &mut P,
    mut parse_argument: impl FnMut(&mut P, usize) -> Result<T, E>,
    mut consume_comma: impl FnMut(&mut P) -> bool,
) -> Result<Vec<T>, E> {
    let mut arguments = Vec::new();
    loop {
        arguments.push(parse_argument(parser, arguments.len())?);
        if !consume_comma(parser) {
            return Ok(arguments);
        }
    }
}

/// Read an identifier without advancing the caller's token cursor.
pub fn parse_identifier<'a>(
    file: &str,
    source: &str,
    token: &'a Token,
) -> Result<&'a str, Diagnostic> {
    match &token.kind {
        Kind::Ident(name) => Ok(name),
        _ => Err(diagnostic_range(file, source, token.offset..token.end,
            "expected identifier").with_code("E0010")),
    }
}

/// Check an expected token while preserving the original byte range.
pub fn expect_token(file: &str, source: &str, token: &Token, expected: Kind) -> Result<(), Diagnostic> {
    if token.kind == expected {
        return Ok(());
    }
    Err(diagnostic_range(file, source, token.offset..token.end,
        &format!("expected {expected:?}, found {:?}", token.kind)).with_code("E0010"))
}
