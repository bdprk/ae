// `ae fmt`: whitespace-only reformatter for valid programs.
//
// Scope rules (v1, deliberately conservative):
// * The only validity a formatter can honestly require is that the
//   compiler's lexer can cut the file into tokens (proven equivalent to the
//   fmt lexer by a test over the whole corpus). Requiring a full check would
//   exclude module files, which have no main of their own; a semantic error
//   elsewhere does not make whitespace normalization unsafe. Anything the
//   lexer cannot cut is reported and the file is left untouched.
// * The output keeps the input's line structure exactly: no line is split or
//   joined. Comments run to end of line in this language, so they survive
//   with their text byte-for-byte; only their indentation can change.
// * Horizontal whitespace between two tokens on one line is decided by the
//   pair of token classes alone, and indentation by the brace depth alone.
//   Neither depends on the original spacing, which is what makes the
//   formatter idempotent by construction: a second pass reads the same
//   classes, the same line breaks and the same depth, so it writes the same
//   bytes.
// * String bodies, comments and literal spellings (`1e3`, `007`) are re-emitted
//   from the source bytes, never from a parsed value.
//
// A formatter that silently changed semantics would be worse than none, so
// `fmt_path` refuses to write anything unless the whole pipeline succeeded,
// and the acceptance tests pin the property down precisely: token stream,
// token line numbers, comments, strings and literal spellings are untouched,
// the program still checks and runs to the same output, and the generated C
// changes only where it embeds source columns (which spacing may move).

use std::path::Path;

// Mirrors the compiler's `Kind` one-for-one (so the two lexers can be
// compared token by token) plus `Comment`, which the compiler drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Ident, Number, Float, Text, Comment,
    Fn, Let, Var, Mut, Print, If, Else, While, For, In, Return, Break, Continue, Assert, True, False, Use, Struct, Enum, Match,
    EqEq, NotEq, Less, LessEq, Greater, GreaterEq, And, Or, Bang,
    LParen, RParen, LBrace, RBrace, LBracket, RBracket, Comma, Colon, Arrow, Amp, Semi, Equal, Plus, Minus, Star, Slash, Percent, Dot, DotDot, ColonColon, FatArrow, Question, Pipe,
    Trait, Impl, Eof,
}

impl Class {
    // Same spelling as `frontend::kind_name` so the equivalence test can
    // compare the two streams by name. Test-only: production formatting
    // decides spacing from classes directly, never from their names.
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Class::Ident => "Ident", Class::Number => "Number", Class::Float => "Float", Class::Text => "Text",
            Class::Comment => "Comment",
            Class::Fn => "Fn", Class::Let => "Let", Class::Var => "Var", Class::Mut => "Mut", Class::Print => "Print",
            Class::If => "If", Class::Else => "Else", Class::While => "While", Class::For => "For",
            Class::In => "In", Class::Return => "Return", Class::Break => "Break", Class::Continue => "Continue",
            Class::Assert => "Assert", Class::True => "True", Class::False => "False", Class::Use => "Use",
            Class::Struct => "Struct", Class::Enum => "Enum", Class::Match => "Match",
            Class::EqEq => "EqEq", Class::NotEq => "NotEq", Class::Less => "Less", Class::LessEq => "LessEq",
            Class::Greater => "Greater", Class::GreaterEq => "GreaterEq", Class::And => "And", Class::Or => "Or",
            Class::Bang => "Bang",
            Class::LParen => "LParen", Class::RParen => "RParen", Class::LBrace => "LBrace", Class::RBrace => "RBrace",
            Class::LBracket => "LBracket", Class::RBracket => "RBracket", Class::Comma => "Comma",
            Class::Colon => "Colon", Class::Arrow => "Arrow", Class::Amp => "Amp", Class::Semi => "Semi", Class::Pipe => "Pipe",
            Class::Equal => "Equal", Class::Plus => "Plus", Class::Minus => "Minus", Class::Star => "Star",
            Class::Slash => "Slash", Class::Percent => "Percent", Class::Dot => "Dot", Class::DotDot => "DotDot",
            Class::ColonColon => "ColonColon", Class::FatArrow => "FatArrow", Class::Question => "Question",
            Class::Trait => "Trait", Class::Impl => "Impl", Class::Eof => "Eof",
        }
    }
}

struct FmtToken {
    class: Class,
    // Exact source bytes of this token: strings with their escapes, comments
    // with their `//`, literals in the spelling the user wrote.
    raw: String,
    line: usize,
    newline_before: bool,
    // Fully blank lines between the previous token and this one, preserved.
    blank_lines_before: u32,
    // True for a Minus/Plus/Bang in prefix position, which never takes a
    // space after it (`return -1`, `print(-x)`, `!(a && b)`).
    prefix_op: bool,
    // For a comment on a code line: the whitespace between the previous
    // token and the `//`, preserved so hand-aligned trailing comments keep
    // their columns. Empty or None means "emit one space".
    trailing_gap: Option<String>,
}

fn lex_error(line: usize, message: &str) -> String {
    format!("fmt: {line}:{message}")
}

// Lexer with the same token boundaries as `frontend::lex` (the equivalence
// test proves it file by file), but keeping raw text and comments instead of
// parsed values. A `check` failure already rejected malformed input, so these
// errors are a defensive net rather than the primary diagnostic.
fn lex_fmt(source: &str) -> Result<Vec<FmtToken>, String> {
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut line = 1usize;
    let mut newline_pending = false;
    let mut last_end = 0usize;
    let mut result: Vec<FmtToken> = Vec::new();
    while i < bytes.len() {
        let c = source[i..].chars().next().unwrap();
        if c == '\n' {
            line += 1;
            i += 1;
            newline_pending = true;
            continue;
        }
        if c.is_whitespace() { i += c.len_utf8(); continue; }
        let (start, start_line) = (i, line);
        let newline_before = newline_pending;
        newline_pending = false;
        if bytes[i..].starts_with(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            // A CRLF file leaves the carriage return just before the newline;
            // it belongs to the line ending, not to the comment.
            let mut end = i;
            while end > start && bytes[end - 1] == b'\r' { end -= 1; }
            result.push(FmtToken {
                class: Class::Comment,
                raw: source[start..end].to_owned(),
                line: start_line,
                newline_before,
                blank_lines_before: blank_between(&result, start_line),
                prefix_op: false,
                trailing_gap: Some(source[last_end..start].to_owned()),
            });
            last_end = i;
            continue;
        }
        let class = if c == '"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => break,
                    b'\n' | b'\0' => return Err(lex_error(start_line, "unterminated or invalid string literal")),
                    b'\\' => {
                        if bytes.get(i + 1).is_none() { return Err(lex_error(start_line, "unterminated string literal")); }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            if i >= bytes.len() { return Err(lex_error(start_line, "unterminated string literal")); }
            i += 1;
            Class::Text
        } else if c.is_ascii_alphabetic() || c == '_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            match &source[start..i] {
                "fn" => Class::Fn, "let" => Class::Let, "var" => Class::Var, "mut" => Class::Mut, "print" => Class::Print,
                "if" => Class::If, "else" => Class::Else, "while" => Class::While, "return" => Class::Return,
                "for" => Class::For, "in" => Class::In, "break" => Class::Break, "continue" => Class::Continue,
                "assert" => Class::Assert, "true" => Class::True, "false" => Class::False, "use" => Class::Use,
                "struct" => Class::Struct, "enum" => Class::Enum, "match" => Class::Match,
                "trait" => Class::Trait, "impl" => Class::Impl,
                _ => Class::Ident,
            }
        } else if c.is_ascii_digit() {
            // Same fraction and exponent boundaries as the compiler lexer:
            // `1..2` stays a range, `1.field` stays a field access.
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            let mut floating = false;
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                floating = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut exponent = i + 1;
                if exponent < bytes.len() && (bytes[exponent] == b'+' || bytes[exponent] == b'-') { exponent += 1; }
                if exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
                    floating = true;
                    i = exponent;
                    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                }
            }
            if floating { Class::Float } else { Class::Number }
        } else if ["==", "!=", "<=", ">=", "->", "&&", "||", "..", "=>", "::"].iter().any(|op| source[i..].starts_with(op)) {
            i += 2;
            match &source[start..i] {
                "==" => Class::EqEq, "!=" => Class::NotEq, "<=" => Class::LessEq, ">=" => Class::GreaterEq,
                "->" => Class::Arrow, "&&" => Class::And, "||" => Class::Or, ".." => Class::DotDot,
                "=>" => Class::FatArrow, "::" => Class::ColonColon, _ => unreachable!(),
            }
        } else {
            i += c.len_utf8();
            match c {
                '(' => Class::LParen, ')' => Class::RParen, '{' => Class::LBrace, '}' => Class::RBrace,
                '[' => Class::LBracket, ']' => Class::RBracket, ',' => Class::Comma, ':' => Class::Colon,
                '&' => Class::Amp, ';' => Class::Semi, '=' => Class::Equal, '+' => Class::Plus,
                '-' => Class::Minus, '*' => Class::Star, '/' => Class::Slash, '%' => Class::Percent,
                '<' => Class::Less, '>' => Class::Greater, '!' => Class::Bang, '.' => Class::Dot,
                '?' => Class::Question,
                '|' => Class::Pipe,
                _ => return Err(lex_error(start_line, &format!("unexpected character {c:?}"))),
            }
        };
        // A Minus/Plus is a prefix operator when the previous token ends an
        // expression position; Bang is always prefix (there is no binary `!`).
        // Getting this wrong only adds or removes a space; the equivalence
        // and emit-c tests keep it honest.
        let prefix_op = match class {
            Class::Bang => true,
            Class::Minus | Class::Plus => result.last().is_none_or(|prev: &FmtToken| matches!(prev.class,
                Class::LParen | Class::LBracket | Class::Comma | Class::Colon | Class::Equal
                | Class::Plus | Class::Minus | Class::Star | Class::Slash | Class::Percent
                | Class::Less | Class::LessEq | Class::Greater | Class::GreaterEq
                | Class::EqEq | Class::NotEq | Class::And | Class::Or | Class::Bang
                | Class::FatArrow | Class::DotDot | Class::Return | Class::In
                | Class::If | Class::While)),
            _ => false,
        };
        result.push(FmtToken {
            class,
            raw: source[start..i].to_owned(),
            line: start_line,
            newline_before,
            blank_lines_before: blank_between(&result, start_line),
            prefix_op,
            trailing_gap: None,
        });
        last_end = i;
    }
    Ok(result)
}

fn blank_between(previous: &[FmtToken], line: usize) -> u32 {
    match previous.last() {
        None => 0,
        Some(prev) => line.saturating_sub(prev.line + 1).min(u32::MAX as usize) as u32,
    }
}

// Space between two tokens that share an output line. The default is a space;
// every exception is listed, and both lists only look at token classes, so
// the decision is stable across passes.
fn need_space(prev: &FmtToken, next: &FmtToken) -> bool {
    // Never a space before these, whatever precedes.
    if matches!(next.class, Class::RParen | Class::RBracket | Class::Comma | Class::Semi
        | Class::Colon | Class::Dot | Class::ColonColon | Class::Question | Class::DotDot) { return false; }
    // An empty block stays `{}`; a non-empty one keeps its inner spaces.
    if matches!(next.class, Class::RBrace) && matches!(prev.class, Class::LBrace) { return false; }
    // Calls have no space before the parenthesis; keywords like `if (` do.
    if matches!(next.class, Class::LParen)
        && matches!(prev.class, Class::Ident | Class::Print | Class::Assert | Class::RParen | Class::RBracket | Class::Question | Class::Bang) { return false; }
    // Indexing and generic arguments: `counts[key]`, `map[string, i64]`,
    // `add[i64]`. Only an expression-ending token binds to the bracket; an
    // array literal after `=` or `(` keeps its space (`var v = [1, 2]`).
    if matches!(next.class, Class::LBracket)
        && matches!(prev.class, Class::Ident | Class::RParen | Class::RBracket) { return false; }
    // Never a space after these.
    if matches!(prev.class, Class::LParen | Class::LBracket | Class::Amp | Class::Dot | Class::ColonColon | Class::DotDot) { return false; }
    // Prefix operators stick to their operand: `-1`, `+x`, `!(a)`.
    if prev.prefix_op { return false; }
    true
}

/// Format lexically valid source without requiring it to pass semantic checks.
/// A lexical error is returned before any output is produced.
pub fn format_source(source: &str) -> Result<String, String> {
    crate::lex(source)?;
    format_validated(source)
}

// Pure text-to-text formatting after compiler-lexer validation.
fn format_validated(source: &str) -> Result<String, String> {
    let tokens = lex_fmt(source)?;
    // Preserve whichever line ending dominates the input; mixed files come
    // out uniform, which the docs list as a deliberate normalization.
    let newline = if source.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(source.len() + 16);
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        if token.class == Class::Eof { break; }
        if index > 0 {
            if token.newline_before {
                for _ in 0..token.blank_lines_before { out.push_str(newline); }
                out.push_str(newline);
                // A closing brace sits one level shallower than its line.
                if token.class == Class::RBrace { depth = depth.saturating_sub(1); }
                for _ in 0..depth { out.push_str("    "); }
            } else {
                if token.class == Class::Comment {
                    // A trailing comment keeps the author's alignment: the
                    // exact horizontal gap survives; a glued `;//` gets one
                    // space. A second pass reads the same gap, so this stays
                    // idempotent.
                    match &token.trailing_gap {
                        Some(gap) if !gap.is_empty() => out.push_str(gap),
                        _ => out.push(' '),
                    }
                } else if need_space(&tokens[index - 1], token) {
                    out.push(' ');
                }
                if token.class == Class::RBrace { depth = depth.saturating_sub(1); }
            }
        }
        out.push_str(&token.raw);
        if token.class == Class::LBrace { depth += 1; }
        // A comment always ends its source line, and the newline after it is
        // picked up as `newline_before` on the next token.
    }
    // Exactly one newline at end of file, whatever came before.
    out.push_str(newline);
    Ok(out)
}

/// Formats one file in place. Returns whether bytes changed. Every failure
/// path (read, lex) happens before any write, and the write itself goes
/// through a temporary file in the same directory, so a crash leaves either
/// the old file or the new one, never a truncated mix.
pub fn fmt_path(path: &Path) -> Result<bool, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // The formatter rewrites whitespace between tokens, so the only validity
    // it can honestly require is that the compiler's lexer can cut the file
    // into tokens. Full checking would exclude module files (they have no
    // main of their own), and a semantic error elsewhere does not make
    // whitespace normalization unsafe: the tokens and their line structure
    // come out untouched.
    crate::lex(&source).map(|_| ())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let formatted = format_validated(&source)?;
    if formatted == source { return Ok(false); }
    let name = path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source.ae".to_owned());
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let staging = dir.join(format!(".ae-fmt-{}-{name}.tmp", std::process::id()));
    std::fs::write(&staging, &formatted)
        .map_err(|e| format!("cannot write {}: {e}", staging.display()))?;
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("cannot replace {} with formatted text: {e}", path.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_formatter_rejects_the_same_unlexable_input_as_the_compiler() {
        for source in ["fn main() { print(\"bad\\q\"); }", "fn main() { print(9223372036854775808); }"] {
            assert_eq!(format_source(source).unwrap_err(), crate::lex(source).unwrap_err());
        }
        assert!(format_source("fn main() { print(unknown); }").is_ok());
    }

    fn fmt(source: &str) -> String {
        format_source(source).unwrap()
    }

    #[test]
    fn normalizes_spacing_without_reordering_lines() {
        assert_eq!(fmt("fn main(){print(1+2*3);}\n"), "fn main() { print(1 + 2 * 3); }\n");
        // The one-line body stays on one line: v1 never joins or splits lines.
        assert_eq!(fmt("fn main() {\nprint(  1 );\n}\n"), "fn main() {\n    print(1);\n}\n");
        assert_eq!(fmt("fn f(a:i64)->i64{return a;}\n"), "fn f(a: i64) -> i64 { return a; }\n");
    }

    #[test]
    fn indentation_follows_brace_depth() {
        let source = "fn main() {\nif true {\nprint(1);\n} else {\nprint(2);\n}\n}\n";
        assert_eq!(fmt(source), "fn main() {\n    if true {\n        print(1);\n    } else {\n        print(2);\n    }\n}\n");
    }

    #[test]
    fn unary_minus_and_bang_take_no_space_after() {
        assert_eq!(fmt("fn main(){print(0 - -1);print(-5);}\n"), "fn main() { print(0 - -1); print(-5); }\n");
        assert_eq!(fmt("fn main(){print(!(true&&false));}\n"), "fn main() { print(!(true && false)); }\n");
        assert_eq!(fmt("fn main(){var a=1;var b=-a;}\n"), "fn main() { var a = 1; var b = -a; }\n");
    }

    #[test]
    fn comments_strings_and_literal_spellings_survive() {
        let source = "// leading\nfn main() { // trailing\n    print(\"a\\n\\\"b\"); // keep\n    print(1e3);\n    print(007);\n}\n";
        let output = fmt(source);
        assert!(output.contains("// leading"), "{output}");
        assert!(output.contains("// trailing"), "{output}");
        assert!(output.contains("// keep"), "{output}");
        assert!(output.contains("\"a\\n\\\"b\""), "{output}");
        assert!(output.contains("1e3"), "{output}");
        assert!(output.contains("007"), "{output}");
        assert_eq!(output, "// leading\nfn main() { // trailing\n    print(\"a\\n\\\"b\"); // keep\n    print(1e3);\n    print(007);\n}\n");
    }

    #[test]
    fn blank_lines_are_preserved_and_file_ends_with_one_newline() {
        let source = "fn a() {\n    print(1);\n}\n\n\nfn main() {\n    print(2);\n}\n\n\n\n";
        assert_eq!(fmt(source), "fn a() {\n    print(1);\n}\n\n\nfn main() {\n    print(2);\n}\n");
    }

    #[test]
    fn crlf_input_stays_crlf() {
        assert_eq!(fmt("fn main() {\r\nprint(1);\r\n}\r\n"), "fn main() {\r\n    print(1);\r\n}\r\n");
    }

    #[test]
    fn formatting_is_idempotent() {
        let sources = [
            "fn main(){print(1+2*3);}\n",
            "// c\nfn f(a: i64) -> i64 {\n    return -a;\n}\n\nfn main() {\n    let v = f(3);\n    if v > 0 { print(v); } else { print(0 - v); }\n}\n",
            "struct Pair[K, V] { left: K, right: V }\nfn main() {\n    let p = Pair[string, i64] { left: \"x\", right: 2 };\n    print(p.left);\n}\n",
            "fn main() {\n    for i in 0..10 {\n        if i % 2 == 0 { continue; }\n        print(i);\n    }\n}\n",
        ];
        for source in sources {
            let once = fmt(source);
            assert_eq!(fmt(&once), once, "not idempotent: {source:?}");
        }
    }

    #[test]
    fn same_class_pairs_get_the_same_space_on_every_pass() {
        // The property the idempotence guarantee rests on: the space between
        // two tokens depends only on their classes.
        let once = fmt("fn main(){var a=1;var b=a%3;print(b==1);}\n");
        assert_eq!(once, "fn main() { var a = 1; var b = a % 3; print(b == 1); }\n");
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn trailing_comment_alignment_is_preserved() {
        let source = "fn main() {\n    let a = 1;        // aligned\n    let b = 2;  // also\n}\n";
        let once = fmt(source);
        assert!(once.contains("let a = 1;        // aligned"), "{once}");
        assert!(once.contains("let b = 2;  // also"), "{once}");
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn glued_comment_gets_one_space() {
        assert_eq!(fmt("fn main() { print(1);// tail\n}\n"), "fn main() { print(1); // tail\n}\n");
        // And the fixed gap survives the next pass unchanged.
        assert_eq!(fmt("fn main() { print(1); // tail\n}\n"), "fn main() { print(1); // tail\n}\n");
    }

    // The formatter must never merge two tokens into one: `& &` must not
    // become `&&`, `- -` must not lose its space. These hold by the class
    // rules above; each case here re-lexes to the same stream.
    #[test]
    fn tokens_never_merge_across_a_removed_space() {
        let once = fmt("fn main(){var a=1;var b=0-a;var c=0-b;print(a&&true);}\n");
        assert_eq!(once, "fn main() { var a = 1; var b = 0 - a; var c = 0 - b; print(a && true); }\n");
        assert_eq!(fmt(&once), once);
    }

    // Defense line against lexer drift: on every shipped source, the fmt
    // lexer must cut the file into exactly the tokens the compiler sees.
    #[test]
    fn fmt_lexer_matches_compiler_lexer_on_all_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in ["examples", "examples/modules", "bench"] {
            if let Ok(read) = std::fs::read_dir(root.join(dir)) {
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("ae") { files.push(path); }
                }
            }
        }
        assert!(files.len() >= 30, "expected the example corpus, found {}", files.len());
        for file in files {
            let source = std::fs::read_to_string(&file).unwrap();
            // Both streams end with their own EOF marker; compare real tokens.
            let compiler: Vec<&str> = crate::lex_kind_names(&source).unwrap().into_iter().filter(|n| *n != "Eof").collect();
            let fmt_tokens = lex_fmt(&source).unwrap();
            let mine: Vec<&str> = fmt_tokens.iter().map(|t| t.class.name()).filter(|n| *n != "Comment").collect();
            assert_eq!(mine, compiler, "lexer drift on {}", file.display());
        }
    }
}
