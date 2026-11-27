//! M8.1.04: the deprecated-alias migration pass.
//!
//! One rule today, the first deprecated alias (M8.1.03):
//!
//! ```text
//! var names = strvec_new();   ->   var names: vec[string] = vec[];
//! let empty = strvec_new();   ->   let empty: vec[string] = vec[];
//! ```
//!
//! The pass is token-based, so text and comments are never rewritten, and it
//! only rewrites the binding form. Any other use (an argument, a field, a
//! returned value) is reported with its position for a manual migration.
//! A second run over the pass's own output changes nothing.
use std::fs;
use std::path::{Path, PathBuf};

use ae_syntax::Kind;

/// What one `ae migrate` run did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub changed_files: usize,
    pub rewritten: usize,
    /// `file:line:column: ...` sites the pass cannot rewrite by itself.
    pub manual: Vec<String>,
    /// `file: reason` failures (unreadable file, lex error, write error).
    pub failed: Vec<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.manual.is_empty() && self.failed.is_empty()
    }
}

pub fn run(inputs: &[PathBuf]) -> Outcome {
    let mut outcome = Outcome::default();
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            collect_dir(input, &mut files, &mut outcome.failed);
        } else if input.is_file() {
            files.push(input.clone());
        } else {
            outcome.failed.push(format!("{}: no such file or directory", input.display()));
        }
    }
    files.sort();
    files.dedup();
    for path in files {
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(err) => {
                outcome.failed.push(format!("{}: {err}", path.display()));
                continue;
            }
        };
        match migrate_source(&source) {
            Ok((rewritten_source, count, manual)) => {
                for (line, column) in manual {
                    outcome.manual.push(format!(
                        "{}:{line}:{column}: 'strvec_new()' is not a binding initializer; write 'var v: vec[string] = vec[]'",
                        path.display()
                    ));
                }
                if count == 0 {
                    continue;
                }
                match fs::write(&path, rewritten_source) {
                    Ok(()) => {
                        outcome.rewritten += count;
                        outcome.changed_files += 1;
                    }
                    Err(err) => outcome.failed.push(format!("{}: {err}", path.display())),
                }
            }
            Err(err) => outcome.failed.push(format!("{}: cannot migrate: {err}", path.display())),
        }
    }
    outcome
}

fn collect_dir(dir: &Path, files: &mut Vec<PathBuf>, failed: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        failed.push(format!("{}: cannot read directory", dir.display()));
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_dir(&child, files, failed);
        } else if child.extension().and_then(|ext| ext.to_str()) == Some("ae") {
            files.push(child);
        }
    }
}

/// The rewritten text, the number of call sites it rewrote, and the
/// `(line, column)` of every alias use the binding rule does not cover.
pub type MigratedSource = (String, usize, Vec<(usize, usize)>);

/// Rewrites one source.
pub fn migrate_source(source: &str) -> Result<MigratedSource, String> {
    let tokens = ae_syntax::lex(source)?;
    let mut edits: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut manual = Vec::new();
    // The binding shape: `var|let Name = strvec_new ( ) ;`.
    let mut index = 0;
    while index < tokens.len() {
        let alias = matches!(&tokens[index].kind, Kind::Ident(name) if name == "strvec_new");
        if !alias {
            index += 1;
            continue;
        }
        let is_call = matches!(tokens.get(index + 1).map(|token| &token.kind), Some(Kind::LParen))
            && matches!(tokens.get(index + 2).map(|token| &token.kind), Some(Kind::RParen));
        let is_binding = is_call
            && index >= 3
            && matches!(tokens[index - 3].kind, Kind::Var | Kind::Let)
            && matches!(tokens[index - 2].kind, Kind::Ident(_))
            && matches!(tokens[index - 1].kind, Kind::Equal)
            && matches!(tokens.get(index + 3).map(|token| &token.kind), Some(Kind::Semi));
        if is_binding {
            // Replace `= strvec_new()` with the annotated empty literal; the
            // name keeps its spelling and the semicolon stays in place.
            edits.push((tokens[index - 2].end, tokens[index + 2].end, ": vec[string] = vec[]"));
            index += 3;
            continue;
        }
        manual.push((tokens[index].line, tokens[index].column));
        index += 1;
    }
    let rewritten = edits.len();
    Ok((apply_edits(source, edits), rewritten, manual))
}

fn apply_edits(source: &str, edits: Vec<(usize, usize, &'static str)>) -> String {
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    for (start, end, text) in edits {
        out.push_str(&source[at..start]);
        out.push_str(text);
        at = end;
    }
    out.push_str(&source[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::migrate_source;

    #[test]
    fn rewrites_the_binding_form_and_keeps_comments() {
        let source = "fn main() {\n    var names = strvec_new(); // keep\n    let empty = strvec_new();\n}\n";
        let (out, count, manual) = migrate_source(source).unwrap();
        assert_eq!(count, 2);
        assert!(manual.is_empty());
        assert!(out.contains("var names: vec[string] = vec[]; // keep"), "{out}");
        assert!(out.contains("let empty: vec[string] = vec[];"), "{out}");
    }

    #[test]
    fn a_second_run_is_a_no_op() {
        let source = "fn main() {\n    var names = strvec_new();\n    push(names, \"a\");\n}\n";
        let (once, count, _) = migrate_source(source).unwrap();
        assert_eq!(count, 1);
        let (twice, count_again, manual_again) = migrate_source(&once).unwrap();
        assert_eq!(count_again, 0);
        assert!(manual_again.is_empty());
        assert_eq!(once, twice);
    }

    #[test]
    fn reports_uses_the_binding_rule_does_not_cover() {
        let source = "fn main() {\n    print(strvec_new());\n}\n";
        let (out, count, manual) = migrate_source(source).unwrap();
        assert_eq!(count, 0);
        assert_eq!(out, source);
        assert_eq!(manual, vec![(2, 11)]);
    }

    #[test]
    fn text_and_comments_are_not_rewritten() {
        let source = "fn main() {\n    print(\"var v = strvec_new();\");\n    // var v = strvec_new();\n    var v = strvec_new();\n}\n";
        let (out, count, manual) = migrate_source(source).unwrap();
        assert_eq!(count, 1);
        assert!(manual.is_empty());
        assert!(out.contains("print(\"var v = strvec_new();\");"), "{out}");
        assert!(out.contains("// var v = strvec_new();"), "{out}");
        assert!(out.contains("var v: vec[string] = vec[];"), "{out}");
    }

    #[test]
    fn a_lex_error_leaves_the_source_alone() {
        assert!(migrate_source("fn main() { \"unterminated }\n").is_err());
    }
}
