// `ae test`: run a file's test functions through the ordinary pipeline.
//
// A test is `fn test_<name>() -> i64`. Returning 0 passes; returning any
// other value fails the suite with that exit code; `assert`, checked
// arithmetic and every other failure path keep their existing behaviour
// (message with line and column, exit status 1).
//
// The driver changes no front-end semantics: it scans the tokens for test
// declarations, appends a synthesized `main` to a copy of the source text,
// and lets the regular check, codegen, cache and run machinery do the rest.
// The copy lives in the same directory as the original, so `use` statements
// resolve exactly as they would for a direct build.

use std::path::{Path, PathBuf};

use crate::frontend::{self, Kind};

// Scans for test declarations in source order. A function whose name starts
// with `test_` must match the convention exactly; anything else named
// `test_*` is an error rather than a silent skip, so a typo'd signature is
// reported instead of leaving the test out of the run.
pub(crate) fn discover(source: &str) -> Result<Vec<String>, String> {
    let tokens = frontend::lex(source)?;
    let mut tests = Vec::new();
    let mut has_main = false;
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].kind == Kind::Fn {
            let name = match tokens.get(i + 1) {
                Some(t) => match &t.kind {
                    Kind::Ident(name) => name.clone(),
                    _ => { i += 1; continue; }
                },
                None => break,
            };
            if name == "main" { has_main = true; }
            if name.starts_with("test_") {
                let line = tokens[i].line;
                let at = |index: usize, kind: &Kind| -> bool {
                    tokens.get(index).is_some_and(|t| &t.kind == kind)
                };
                let is_i64 = |index: usize| -> bool {
                    tokens.get(index).is_some_and(|t| matches!(&t.kind, Kind::Ident(text) if text == "i64"))
                };
                let generic = at(i + 2, &Kind::LBracket);
                let zero_params = at(i + 2, &Kind::LParen) && at(i + 3, &Kind::RParen);
                let returns_i64 = at(i + 4, &Kind::Arrow) && is_i64(i + 5);
                if generic || !zero_params {
                    return Err(format!("{line}: test function {name} must take no parameters; expected `fn {name}() -> i64`"));
                }
                if !returns_i64 {
                    return Err(format!("{line}: test function {name} must return i64; expected `fn {name}() -> i64`"));
                }
                if !tests.contains(&name) { tests.push(name); }
            }
        }
        i += 1;
    }
    if has_main {
        return Err("test files must not define main; run programs with `ae run`".to_owned());
    }
    if tests.is_empty() {
        return Err("no test functions found; a test is `fn test_<name>() -> i64` returning 0 on success".to_owned());
    }
    Ok(tests)
}

// The synthesized driver. Each test prints RUN before it is called, PASS
// after it returned 0, or FAIL plus its value as the process exit code. A
// test that panics never reaches its PASS line, so the last printed line
// names the test that died.
pub(crate) fn synthetic_main(tests: &[String]) -> String {
    let mut out = String::from("\n// synthesized by `ae test`\nfn main() {\n");
    for (index, name) in tests.iter().enumerate() {
        let slot = format!("_ae_test_{index}");
        out.push_str(&format!("    print(\"RUN {name}\");\n"));
        out.push_str(&format!("    let {slot} = {name}();\n"));
        out.push_str(&format!("    if {slot} != 0 {{ print(\"FAIL {name}\"); exit({slot}); }}\n"));
        out.push_str(&format!("    print(\"PASS {name}\");\n"));
    }
    out.push_str(&format!("    print(\"ok: {} passed\");\n", tests.len()));
    out.push_str("}\n");
    out
}

/// The staged test program: a copy of the source with the driver appended,
/// written next to the original so relative `use` paths keep working. The
/// file is removed when this value drops, on every path.
pub struct TestProgram {
    pub staging: PathBuf,
    pub tests: Vec<String>,
}

impl Drop for TestProgram {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.staging); }
}

pub fn prepare(path: &Path, filter: Option<&str>) -> Result<TestProgram, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut tests = discover(&source).map_err(|e| format!("{}: {e}", path.display()))?;
    // A filter is a plain substring on the test name (ADR-62): `ae test
    // file.ae trim` runs only the tests whose names contain `trim`. An empty
    // match is an error, not a green run — a typo'd filter must not look like
    // everything passed.
    if let Some(filter) = filter {
        tests.retain(|name| name.contains(filter));
        if tests.is_empty() {
            return Err(format!("no test functions match filter '{filter}'"));
        }
    }
    let name = path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tests.ae".to_owned());
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let staging = dir.join(format!(".ae-test-{}-{name}", std::process::id()));
    std::fs::write(&staging, source.clone() + &synthetic_main(&tests))
        .map_err(|e| format!("cannot write {}: {e}", staging.display()))?;
    Ok(TestProgram { staging, tests })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_tests_in_declaration_order() {
        let source = "fn helper() -> i64 { return 1; }\nfn test_b() -> i64 { return 0; }\nfn test_a() -> i64 { return 0; }\n";
        assert_eq!(discover(source).unwrap(), vec!["test_b".to_owned(), "test_a".to_owned()]);
    }

    #[test]
    fn rejects_test_functions_that_break_the_convention() {
        for broken in [
            "fn test_x(y: i64) -> i64 { return y; }\n",
            "fn test_x() -> string { return \"s\"; }\n",
            "fn test_x() { }\n",
            "fn test_x[T]() -> i64 { return 0; }\n",
        ] {
            let message = discover(broken).unwrap_err();
            assert!(message.contains("fn test_x() -> i64"), "{broken:?}: {message}");
        }
    }

    #[test]
    fn filter_selects_substrings_and_refuses_empty_matches() {
        let path_source = "fn test_trim_start() -> i64 { return 0; }
fn test_trim_end() -> i64 { return 0; }
fn test_join() -> i64 { return 0; }
";
        let dir = std::env::temp_dir().join(format!("ae-harness-filter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.ae");
        std::fs::write(&file, path_source).unwrap();
        let picked = prepare(&file, Some("trim")).unwrap();
        assert_eq!(picked.tests, vec!["test_trim_start".to_owned(), "test_trim_end".to_owned()]);
        let text = std::fs::read_to_string(&picked.staging).unwrap();
        assert!(text.contains("RUN test_trim_start") && text.contains("RUN test_trim_end"), "{text}");
        assert!(!text.contains("RUN test_join"), "{text}");
        assert!(text.contains("ok: 2 passed"), "{text}");
        drop(picked);
        let message = match prepare(&file, Some("unicode")) {
            Err(message) => message,
            Ok(program) => { drop(program); panic!("empty filter match should fail"); }
        };
        assert!(message.contains("no test functions match filter 'unicode'"), "{message}");
        let everything = prepare(&file, None).unwrap();
        assert_eq!(everything.tests.len(), 3);
        drop(everything);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_files_with_main_or_without_tests() {
        let message = discover("fn main() { print(1); }\nfn test_a() -> i64 { return 0; }\n").unwrap_err();
        assert!(message.contains("must not define main"), "{message}");
        let message = discover("fn helper() -> i64 { return 1; }\n").unwrap_err();
        assert!(message.contains("no test functions"), "{message}");
    }

    #[test]
    fn synthesized_main_runs_tests_in_order_and_propagates_failures() {
        let text = synthetic_main(&["test_a".to_owned(), "test_b".to_owned()]);
        assert!(text.contains("print(\"RUN test_a\");"), "{text}");
        assert!(text.contains("let _ae_test_0 = test_a();"), "{text}");
        assert!(text.contains("if _ae_test_0 != 0 { print(\"FAIL test_a\"); exit(_ae_test_0); }"), "{text}");
        assert!(text.contains("print(\"ok: 2 passed\");"), "{text}");
        // The synthesized code is ordinary ae: fmt must leave it
        // unchanged apart from the documented leading-blank-line rule (the
        // leading newline only separates the driver from the user's source).
        assert_eq!(crate::fmt::format_source(&text).unwrap(), text.trim_start_matches('\n'));
    }
}
