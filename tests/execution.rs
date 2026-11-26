use std::{fs, process::Command, sync::atomic::{AtomicUsize, Ordering}};
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// ADR-0224 R3a: the shared statement grammar must accept every function body
/// the compiler accepts. The corpus is the same run/example tree the other
/// suites use, so a new statement shape cannot land without the shared parser
/// learning it (or this test failing loudly).
#[test]
fn shared_statement_grammar_accepts_every_compiler_program() {
    use ae_syntax::declaration::{parse_declarations, DeclarationKind};
    use ae_syntax::statement::parse_block;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    for dir in ["tests/run", "examples"] {
        let base = root.join(dir);
        let mut stack = vec![base];
        while let Some(path) = stack.pop() {
            let Ok(entries) = fs::read_dir(&path) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "ae") {
                    sources.push(path);
                }
            }
        }
    }
    sources.sort();
    assert!(sources.len() > 100, "the corpus must be the real one, found {}", sources.len());

    let mut failures = Vec::new();
    let mut bodies = 0usize;
    for path in &sources {
        let source = fs::read_to_string(path).unwrap_or_default();
        let label = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let declarations = match parse_declarations(&label, &source) {
            Ok(declarations) => declarations,
            // A few corpus files are deliberate rejections; the compiler owns
            // those verdicts and the shared grammar does not have to match them
            // yet. The run corpus is the acceptance set.
            Err(error) => {
                if label.starts_with("tests/run/") {
                    failures.push(format!("{label}: declaration parse failed: {}", error.message));
                }
                continue;
            }
        };
        for declaration in declarations {
            let DeclarationKind::Function { .. } = declaration.kind else { continue };
            let Some(body) = declaration.body else { continue };
            // `body` covers the braces; find the opening one inside it.
            let Some(open) = source[body.clone()].find('{') else {
                failures.push(format!("{label}: function body has no opening brace"));
                continue;
            };
            let absolute = body.start + open;
            let tokens = match ae_syntax::lex_diagnostic(&source) {
                Ok(tokens) => tokens,
                Err(error) => {
                    failures.push(format!("{label}: lex failed: {}", error.message));
                    continue;
                }
            };
            let start = tokens.iter().position(|token| token.offset == absolute);
            let Some(start) = start else {
                failures.push(format!("{label}: body brace is not a token boundary"));
                continue;
            };
            match parse_block(&label, &source, &tokens, start) {
                Ok((_, end)) => {
                    bodies += 1;
                    if tokens[end].offset < body.end - 1 {
                        failures.push(format!(
                            "{label}: statements stopped at byte {} but the body ends at {}",
                            tokens[end].offset,
                            body.end - 1
                        ));
                    }
                }
                Err(error) => {
                    let where_ = error.primary.as_ref().map(|primary| {
                        let start = primary.span.range.start;
                        let line = source[..start.min(source.len())].matches('\n').count() + 1;
                        let line_text = source.lines().nth(line - 1).unwrap_or("").trim();
                        format!("{label}:{}: {} | {}", line, error.message, line_text)
                    });
                    failures.push(where_.unwrap_or_else(|| format!("{label}: {}", error.message)));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} bodies failed:\n{}", failures.len(), bodies, failures.join("\n"));
    assert!(bodies > 150, "expected a large body corpus, parsed {bodies}");
}

/// R3a differential gate: the shared statement grammar must split every
/// statement of an accepted program exactly where the compiler does. The
/// compiler still builds the typed tree; this checks that switching the
/// boundary finder would not change any verdict.
#[test]
fn shared_statement_grammar_splits_accepted_statements_where_the_compiler_does() {
    use ae_syntax::declaration::{parse_declarations, DeclarationKind};
    use ae_syntax::statement::{parse_statement_tokens, StatementKind};

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;
    let mut mismatches = Vec::new();
    for dir in ["tests/run", "examples"] {
        let mut stack = vec![root.join(dir)];
        while let Some(path) = stack.pop() {
            let Ok(entries) = fs::read_dir(&path) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "ae") {
                    continue;
                }
                let label = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                let source = fs::read_to_string(&path).unwrap_or_default();
                // Only programs the compiler itself accepts belong in this gate.
                let verdict = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
                if !verdict.status.success() {
                    continue;
                }
                let Ok(declarations) = parse_declarations(&label, &source) else { continue };
                let tokens = ae_syntax::lex_diagnostic(&source).unwrap();
                for declaration in declarations {
                    let DeclarationKind::Function { .. } = declaration.kind else { continue };
                    let Some(body) = declaration.body else { continue };
                    let Some(open) = source[body.clone()].find('{') else { continue };
                    let Some(mut cursor) = tokens.iter().position(|token| token.offset == body.start + open) else { continue };
                    cursor += 1;
                    loop {
                        if tokens[cursor].kind == ae_syntax::Kind::RBrace {
                            break;
                        }
                        let start = cursor;
                        let (statement, end) = match parse_statement_tokens(&label, &source, &tokens, cursor) {
                            Ok(parsed) => parsed,
                            Err(error) => {
                                mismatches.push(format!("{label}: {}", error.message));
                                break;
                            }
                        };
                        // A statement must consume at least one token and end at
                        // a token boundary inside the body.
                        if end <= start {
                            mismatches.push(format!("{label}: statement at token {start} consumed nothing"));
                            break;
                        }
                        if matches!(statement.kind, StatementKind::Expr { .. }) && !statement.kind.is_effect_statement() {
                            // The compiler rejects a bare effect call; an
                            // accepted program cannot contain one, so reaching
                            // here means the splitter drifted.
                            mismatches.push(format!("{label}: accepted program has a non-propagating call"));
                        }
                        checked += 1;
                        cursor = end;
                    }
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    assert!(checked > 1000, "expected a large statement corpus, checked {checked}");
}

fn execute(source: &str) -> std::process::Output {
    let path = std::env::temp_dir().join(format!("ae-integration-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    output
}
#[test]
fn values_and_precedence_execute() {
    let output = execute("fn main() { let x = 2; var y = x + 3 * 4; y = y / 2; print(y); print(0 - 5); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n-5\n");
}
#[test]
fn arithmetic_failures_are_reported() {
    for (expr, expected) in [("9223372036854775807 + 1", "integer overflow"), ("1 / 0", "division by zero"), ("3037000500 * 3037000500", "integer overflow"), ("(0 - 9223372036854775807 - 1) / (0 - 1)", "integer overflow")] {
        let output = execute(&format!("fn main() {{ print({expr}); }}"));
        assert!(!output.status.success(), "{expr}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{expr}: {}", String::from_utf8_lossy(&output.stderr));
    }
}
#[test]
fn build_accepts_non_ascii_absolute_output_paths() {
    // The compiler used to pass the temporary directory to gcc as an absolute
    // path. On Windows the MinGW toolchain converts the command line to the ANSI
    // code page, so a path like `...\用户\...` reached the linker mangled and
    // the build failed with "cannot open output file" — invisible whenever the
    // cache answered first, because then gcc never ran. The temporary files are
    // now named relative to the compiler's working directory.
    let dir = unique_temp_dir("nonascii");
    let cjk = dir.join("中文目录");
    fs::create_dir(&cjk).unwrap();
    let source = dir.join("hello.ae");
    fs::write(&source, "fn main() { print(42); }\n").unwrap();
    let cache = dir.join("cache");
    fs::create_dir(&cache).unwrap();
    let output = cjk.join(if cfg!(windows) { "程序.exe" } else { "程序" });
    let out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("build").arg(&source).arg("-o").arg(&output)
        .env("AE_CACHE", &cache)
        .output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(output.is_file(), "missing {}", output.display());
    let run = Command::new(&output).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n").trim(), "42");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn build_creates_executable_and_refuses_to_overwrite() {
    let dir = std::env::temp_dir().join(format!("ae-build-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("程序.ae");
    let executable = dir.join(if cfg!(windows) { "result.exe" } else { "result" });
    fs::write(&input, "fn main() { print(123); }").unwrap();
    let build = || Command::new(env!("CARGO_BIN_EXE_ae")).arg("build").arg(&input).arg("-o").arg(&executable).output().unwrap();
    let result = build();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let output = Command::new(&executable).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "123");
    let before = fs::read(&executable).unwrap();
    let result = build();
    assert!(!result.status.success());
    assert_eq!(fs::read(&executable).unwrap(), before);
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn check_error_identifies_file_and_location() {
    let path = std::env::temp_dir().join(format!("ae-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { print(x); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!result.status.success());
    let message = String::from_utf8_lossy(&result.stderr);
    assert!(message.contains("1:19:"), "{message}");
    assert!(message.contains("unknown variable"), "{message}");
}
#[test]
fn control_flow_and_scopes_execute() {
    let source = "fn main() { var n = 0; var sum = 0; while n < 5 { let value = n * 2; sum = sum + value; n = n + 1; } if sum == 20 { let result = sum + 1; print(result); } else { print(0); } if n != 5 { print(999); } else { print(n); } }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "21\n5\n");
}
#[test]
fn loop_conditions_are_rechecked_and_short_branches_are_not_executed() {
    let source = "fn main() { var n = 2; while n > 0 { n = n - 1; } if n >= 1 { print(1 / 0); } else { print(n); } }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
}
#[test]
fn block_variables_do_not_escape() {
    let path = std::env::temp_dir().join(format!("ae-scope-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { if 1 == 1 { let secret = 1; } print(secret); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown variable 'secret'"));
}
#[test]
fn excessive_nesting_returns_diagnostic_instead_of_crashing() {
    let nested = format!("fn main() {{ print({}1{}); }}", "(".repeat(300), ")".repeat(300));
    let output = execute(&nested);
    assert!(!output.status.success());
    // Deep parentheses: the typed route says "nesting limit", the shared
    // grammar "complexity limit"; the contract is that a limit fires.
    assert!(String::from_utf8_lossy(&output.stderr).contains("limit exceeded"));
    let long = format!("fn main() {{ print({}); }}", vec!["1"; 300].join(" + "));
    let output = execute(&long);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("complexity limit"));
    let blocks = format!("fn main() {{ {} print(1); {} }}", "if 1 == 1 { ".repeat(100), "}".repeat(100));
    let output = execute(&blocks);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("limit exceeded"));
}
#[test]
fn signed_arithmetic_boundaries_match_checked_integer_semantics() {
    let success = execute("fn main() { print((0 - 9223372036854775807 - 1) * 1); print((0 - 2) * (0 - 3)); print((0 - 7) / 3); print(9223372036854775807 + (0 - 1)); }");
    assert!(success.status.success(), "{}", String::from_utf8_lossy(&success.stderr));
    assert_eq!(String::from_utf8_lossy(&success.stdout).replace("\r\n", "\n"), "-9223372036854775808\n6\n-2\n9223372036854775806\n");
    for expr in ["(0 - 9223372036854775807 - 1) - 1", "(0 - 9223372036854775807 - 1) * (0 - 1)", "(0 - 3037000500) * 3037000500", "9223372036854775807 - (0 - 1)"] {
        let output = execute(&format!("fn main() {{ print({expr}); }}"));
        assert!(!output.status.success(), "{expr}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("integer overflow"), "{expr}");
    }
}
#[test]
fn runtime_error_reports_the_operator_location() {
    let output = execute("fn main() {\n  let x = 0;\n  print(7 / x);\n}");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("ae:3:11: division by zero"), "{error}");
}
#[test]
fn sibling_blocks_reuse_names_without_affecting_outer_scope() {
    let source = "fn main() { var outer = 1; if outer == 1 { let local = 3; outer = local; } if outer == 3 { let local = 8; print(local); } print(outer); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "8\n3\n");
}
#[test]
fn release_mode_preserves_checked_arithmetic_and_output() {
    let dir = std::env::temp_dir().join(format!("ae-release-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let source = dir.join("input.ae");
    let output = dir.join(if cfg!(windows) { "result.exe" } else { "result" });
    fs::write(&source, "fn main() { var x = 9223372036854775807; print(x + 1); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("build").arg(&source).arg("-o").arg(&output).arg("--release").output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let result = Command::new(&output).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("integer overflow"));
    fs::write(&source, "fn main() { var x = 2; while x < 5 { x = x + 1; } print(x); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).arg("--release").output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "5");
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn build_is_optimized_by_default_and_debug_builds_behave_the_same() {
    // ADR-59: `build` writes the artifact a user ships, so it defaults to -O2;
    // --debug asks for the fast -O0 build. Both keep every check and must
    // produce the same output.
    let dir = std::env::temp_dir().join(format!("ae-build-levels-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let source = dir.join("input.ae");
    let optimized = dir.join(if cfg!(windows) { "optimized.exe" } else { "optimized" });
    let debug = dir.join(if cfg!(windows) { "debug.exe" } else { "debug" });
    fs::write(&source, "fn main() { var total = 0; for i in 0..1000 { total = total + i; } print(total); }").unwrap();
    for (flag, output) in [(None, &optimized), (Some("--debug"), &debug)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("build").arg(&source).arg("-o").arg(output);
        if let Some(flag) = flag { command.arg(flag); }
        let result = command.output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let result = Command::new(output).output().unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "499500");
    }
    // An unknown flag is still refused instead of being ignored.
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("build").arg(&source).arg("-o").arg(dir.join("nope")).arg("--faster").output().unwrap();
    assert!(!result.status.success());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn run_forwards_program_args_with_the_release_flag_before_the_separator() {
    let path = std::env::temp_dir().join(format!("ae-run-forward-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { let a = args(); print(len(a)); print(a[1]); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--release").arg("--").arg("hello").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).replace("\r\n", "\n"), "2\nhello\n");
}
#[test]
fn math_builtins_compute_report_domain_errors() {
    // ADR-60: thin libm calls. Exact results print identically everywhere;
    // domain errors are IEEE (NaN), and the self-formatted text is stable.
    let source = "fn main() { print(sqrt(4.0)); print(abs(-2.5)); print(floor(2.7)); print(ceil(2.1)); print(round(2.5)); print(round(-2.5)); print(pow(2.0, 10.0)); print(exp(0.0)); print(log(1.0)); print(sin(0.0)); print(cos(0.0)); print(sqrt(-1.0)); print(to_string(sqrt(-1.0)) == \"nan\"); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2.0\n2.5\n2.0\n3.0\n3.0\n-3.0\n1024.0\n1.0\n0.0\n0.0\n1.0\nnan\ntrue\n");
}
#[test]
fn math_transcendental_values_land_within_a_tolerance() {
    // libm implementations may differ by an ulp across platforms, so the
    // portable contract is closeness, not byte-identical transcendental text.
    let source = "fn close(a: f64, b: f64) -> bool { let gap = abs(a - b); return gap < 0.000000000000001; } fn main() { print(close(sqrt(2.0), 1.4142135623730951)); print(close(sin(1.0), 0.8414709848078965)); print(close(cos(1.0), 0.5403023058681398)); print(close(atan2(1.0, 1.0), 0.7853981633974483)); print(close(log(exp(1.0)), 1.0)); print(close(pow(2.0, 0.5), sqrt(2.0))); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\ntrue\ntrue\ntrue\ntrue\ntrue\n");
}
#[test]
fn min_max_ignore_nan_and_tie_zeros_deterministically() {
    // ADR-64: runtime min/max, not libm — one NaN yields the other, the ±0.0
    // tie follows totalOrder, so printed text is identical on every platform
    // and independent of argument order.
    let source = "fn main() { print(min(2.0, 3.0)); print(max(2.0, 3.0)); print(min(-0.0, 0.0)); print(min(0.0, -0.0)); print(max(-0.0, 0.0)); print(max(0.0, -0.0)); print(min(sqrt(-1.0), 5.0)); print(min(5.0, sqrt(-1.0))); print(max(sqrt(-1.0), 5.0)); print(max(sqrt(-1.0), sqrt(-1.0))); print(min(1.5, 1.5)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2.0\n3.0\n-0.0\n-0.0\n0.0\n0.0\n5.0\n5.0\n5.0\nnan\n1.5\n");
}
#[test]
fn min_max_reject_wrong_types_and_redefinition() {
    for (source, expected) in [
        ("fn main() { print(min(1, 2)); }", "min expects an f64 argument"),
        ("fn main() { print(max(1.0)); }", "max expects 2 f64 argument(s)"),
        ("fn min(x: f64, y: f64) -> f64 { return x; } fn main() { print(min(1.0, 2.0)); }", "is a built-in"),
        ("fn main() { var max = 1.0; print(max); }", "is a built-in name and cannot be a variable"),
    ] {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}: {message}");
    }
}
#[test]
fn math_builtins_reject_wrong_types_arities_and_redefinition() {
    for (source, expected) in [
        ("fn main() { print(sqrt(4)); }", "sqrt expects an f64 argument"),
        ("fn main() { print(abs(true)); }", "abs expects an f64 argument"),
        ("fn main() { print(atan2(1.0)); }", "atan2 expects 2 f64 argument(s)"),
        ("fn main() { print(pow(1.0, 2.0, 3.0)); }", "pow takes 2 argument(s)"),
        ("fn sqrt(x: f64) -> f64 { return x; } fn main() { print(sqrt(1.0)); }", "is a built-in"),
        ("fn main() { var sqrt = 1.0; print(sqrt); }", "is a built-in name and cannot be a variable"),
    ] {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}: {message}");
    }
}
#[test]
fn float_vectors_store_index_and_iterate() {
    // ADR-61: vec[f64] behaves like vec with f64 elements — literal, checked
    // indexing, len, push/pop, and by-value for-in iteration.
    let source = "fn mean(values: &vec[f64]) -> f64 { var total = 0.0; for x in values { total = total + x; } return total / to_f64(len(values)); } fn main() { let v = vec[2.0, 4.0, 6.0]; print(len(v)); print(v[0]); print(v[2]); var w = vec[2.0, 4.0, 6.0]; w[1] = 5.5; print(w[1]); print(v[1]); push(w, 8.0); print(len(w)); pop(w); print(len(w)); var acc = f64vec_new(); push(acc, 1.5); push(acc, 2.5); print(len(acc)); print(acc[1]); print(mean(&v)); print(mean(&w)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // mean(v)=4.0; mean(w) with w[1]=5.5 and no trailing 8.0 → (2+5.5+6)/3=4.5
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n2.0\n6.0\n5.5\n4.0\n4\n3\n2\n2.5\n4.0\n4.5\n");
}
#[test]
fn float_vectors_move_release_and_borrow_like_other_owned_types() {
    // Moves empty the source slot; borrows are read-only and refuse writes
    // while active; an early return still releases the live vector.
    let source = "fn consume(values: vec[f64]) -> f64 { if len(values) > 1 { return values[0] + values[1]; } return 0.0; } fn read_only(values: &vec[f64]) -> f64 { return values[1]; } fn maybe(keep: bool) -> f64 { var v = vec[10.0, 20.0, 30.0]; if !keep { return v[0]; } return v[2]; } fn main() { let v = vec[1.0, 2.0, 3.0]; print(read_only(&v)); print(consume(v)); print(maybe(true)); print(maybe(false)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2.0\n3.0\n30.0\n10.0\n");
}
#[test]
fn float_vector_misuse_is_rejected() {
    for (source, expected) in [
        ("fn main() { let v = vec[1, 2.0]; }", "does not convert implicitly"),
        ("enum E { A(string) } fn main() { let v = vec[E::A(\"a\")]; }", "vector literal elements must be a value type"),
        ("fn main() { var v = vec[1.0]; push(v, 2); }", "vector elements must be f64"),
        ("fn main() { var v = vec[1.0]; sort(v); }", "not supported for vec[f64]"),
        ("fn main() { var v = vec[1.0]; reverse(v); }", "not supported for vec[f64]"),
        ("fn main() { let v = vec[1.0]; print(v[0.5]); }", "index must be an integer"),
        ("fn main() { let v = vec[1.0, 2.0]; let w = v; print(v[0]); }", "moved"),
        ("fn main() { var v = vec[1.0]; v[0] = 1; }", "vector elements must be f64"),
        ("fn main() { var v = vec[1.0]; for x in v { v[0] = 2.0; } }", "while it is borrowed"),
        ("fn main() { var v = f64vec_new(); let x = v; push(v, 1.0); }", "moved"),
        ("fn main() { let v = vec[1.0]; let x = v + 1.0; }", "mixed i64/f64 arithmetic"),
    ] {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}: {message}");
    }
}
#[test]
fn float_vectors_live_in_structs_and_enums_and_release_exactly_once() {
    // Struct fields of vec[f64] type release with the struct (leak check runs
    // in the generated-C suite); enum payloads bind as read-only vec[f64]
    // borrows in the match arm, where element access works on the binding.
    let source = "struct Sample { values: vec[f64], label: string } enum Reading { Wave(vec[f64]), None } fn main() { var s = Sample { values: vec[1.0, 2.0], label: \"a\" }; print(s.label); let r = Reading::Wave(vec[3.5, 4.5]); match r { Reading::Wave(w) => { print(w[0] + w[1]); } Reading::None => { print(0.0); } } print(\"done\"); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a\n8.0\ndone\n");
}
#[test]
fn arrays_support_checked_read_write_and_expression_order() {
    let source = "fn main() { let original = [3, 4]; var values = [original[1], 5 + 2]; values[1] = values[0] + values[1]; print(values[1]); print(original[0]); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "11\n3\n");
    for expr in ["print(values[2]);", "print(values[0 - 1]);", "values[2] = 1 / 0;"] {
        let output = execute(&format!("fn main() {{ var values = [7, 8]; {expr} }}"));
        assert!(!output.status.success(), "{expr}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("array index out of bounds"), "{expr}: {}", String::from_utf8_lossy(&output.stderr));
    }
}
#[test]
fn array_type_and_mutability_errors_are_rejected() {
    for source in ["fn main() { let a = [1]; a[0] = 2; }", "fn main() { let a = []; }", "fn main() { let a = [1]; print(a); }", "fn main() { var a = [1]; a = 1; }", "fn main() { let a = 1; print(a[0]); }"] {
        let path = std::env::temp_dir().join(format!("ae-array-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!result.status.success(), "{source}");
    }
}
#[test]
fn release_array_bounds_checks_remain_enabled() {
    let path = std::env::temp_dir().join(format!("ae-array-release-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { let values = [4, 5]; print(values[2]); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--release").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("array index out of bounds"));
}

/// `for x in a` iterates a fixed array: the length is the type's constant,
/// elements bind by value, and the array is borrowed for the loop's duration.
#[test]
fn array_for_matches_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("array_for.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    // The iterated array is borrowed for the whole loop, so an element store
    // inside the body is rejected exactly like the vector case.
    let path = std::env::temp_dir().join(format!(
        "ae-array-for-bad-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, "fn main() { var a = [1, 2]; for x in a { a[0] = 9; print(x); } }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!result.status.success(), "an element store inside the array loop must be rejected");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("cannot modify 'a' while it is borrowed"),
        "got {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// A fixed array is a Copy value: copy initialization, whole-array
/// assignment and compound element stores copy element by element, and a
/// compound store evaluates its index once.
#[test]
fn array_copy_and_store_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("array_copy.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "compound store on a let array",
            "fn main() { let a = [1, 2]; a[0] += 1; }",
            "cannot assign to immutable variable 'a'",
        ),
        (
            "whole assignment on a let array",
            "fn main() { let a = [1, 2]; a = [3, 4]; }",
            "cannot assign to immutable variable 'a'",
        ),
        (
            "assignment inside the loop that borrows it",
            "fn main() { var a = [1, 2]; for x in a { a = [3, 4]; print(x); } }",
            "cannot modify 'a' while it is borrowed",
        ),
        (
            "length mismatch",
            "fn main() { var a = [1, 2, 3]; a = [1, 2]; }",
            "array length mismatch: expected 3, found 2",
        ),
        (
            "literal element mismatch",
            "fn main() { var a = [1, 2]; a = [1.5, 2.5]; }",
            "array element type mismatch: expected i64, found f64",
        ),
        (
            "binding type mismatch",
            "fn main() { var a = [1, 2]; let b = [1.5, 2.5]; a = b; }",
            "array type mismatch",
        ),
        (
            "compound store on bool elements",
            "fn main() { var a = [true, false]; a[0] += true; }",
            "compound assignment is not supported for type 'bool'",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-array-copy-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
#[test]
fn generated_integer_program_matches_independent_reference_in_both_modes() {
    // Bounded operands avoid reference overflow, while exercising signed arithmetic,
    // precedence, arrays, repeated assignment, and both C optimization modes.
    let mut state = 0x52fe_7062_d18a_9387u64;
    let mut source = String::from("fn main() { var a = [0, 0, 0];\n");
    let mut expected = String::new();
    for i in 0..250 {
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        let a = (state % 20001) as i64 - 10000;
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        let b = (state % 20001) as i64 - 10000;
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        let c = (state % 200 + 1) as i64;
        let repr = |n: i64| if n < 0 { format!("(0 - {})", -n) } else { n.to_string() };
        let index = i % 3;
        source.push_str(&format!("a[{index}] = ({} * {} + {}) / {}; print(a[{index}]);\n", repr(a), repr(b), repr(a), c));
        expected.push_str(&format!("{}\n", (a * b + a) / c));
    }
    source.push_str("}\n");
    let path = std::env::temp_dir().join(format!("ae-diff-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    for release in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&path);
        if release { command.arg("--release"); }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), expected);
    }
    fs::remove_file(path).unwrap();
}
#[test]
fn opt_in_timings_preserve_program_stdout() {
    let path = std::env::temp_dir().join(format!("ae-timing-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { print(42); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).env("AE_TIMINGS", "1").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("frontend_and_codegen_ms="), "{err}");
    assert!(err.contains("c_compile_and_link_ms="), "{err}");
}
#[test]
fn check_does_not_require_c_compiler() {
    let path = std::env::temp_dir().join(format!("ae-check-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() { let a = [1, 2]; print(a[0]); }").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).env("AE_CC", "nonexistent-ae-compiler").env("AE_TIMINGS", "1").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("frontend_ms="));
    assert!(!stderr.contains("c_compile_and_link_ms="));
}
#[test]
fn functions_execute_recursion_and_ordered_arguments() {
    let source = "fn add(a: i64, b: i64) -> i64 { return a + b; } fn fact(n: i64) -> i64 { if n <= 1 { return 1; } else { return n * fact(n - 1); } } fn main() { print(add(fact(5), 3)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "123");
    let output = execute("fn f(a: i64, b: i64) -> i64 { return a + b; } fn main() { print(f(1 / 0, 9223372036854775807 + 1)); }");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(err.contains("division by zero"), "{err}");
}
#[test]
fn invalid_functions_are_rejected() {
    for source in [
        "fn f(x: i64) -> i64 { if x > 0 { return 1; } } fn main() {}",
        "fn f(x: i64) -> i64 { x = 2; return x; } fn main() {}",
        "fn f(x: i64) -> i64 { return x; } fn main() { print(f()); }",
        "fn f() -> i64 { return 1; } fn f() -> i64 { return 2; } fn main() {}",
        "fn main() { return 1; }", "fn main(x: i64) {}",
    ] {
        let path = std::env::temp_dir().join(format!("ae-fn-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!result.status.success(), "{source}");
    }
}
#[test]
fn function_calls_evaluate_arguments_left_to_right_even_with_side_effects() {
    let source = "fn show(x: i64) -> i64 { print(x); return x; } fn subtract(a: i64, b: i64) -> i64 { return a - b; } fn main() { print(subtract(show(7), show(3))); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n3\n4\n");
}
#[test]
fn nested_return_paths_and_early_return_behave_consistently() {
    let source = "fn classify(x: i64) -> i64 { if x < 0 { return 1; } else { if x == 0 { return 2; } else { return 3; } } } fn early(x: i64) -> i64 { if x == 0 { return 4; } return 5; } fn main() { print(classify(0 - 1)); print(classify(0)); print(classify(2)); print(early(0)); print(early(3)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n2\n3\n4\n5\n");
}
#[test]
fn emitted_c_passes_strict_return_type_check() {
    let path = std::env::temp_dir().join(format!("ae-c-check-{}-{}.c", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/functions.ae");
    let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(source).output().unwrap();
    assert!(generated.status.success());
    fs::write(&path, generated.stdout).unwrap();
    let cc = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let output = Command::new(cc).args(["-std=c11", "-pedantic-errors", "-Werror=return-type", "-fsyntax-only"]).arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
#[test]
fn scalar_programs_link_without_the_allocator_and_string_programs_keep_it() {
    // M3.1.01 (charter rule 8): a program that never allocates carries none
    // of the allocator — the argv conversion included, which now happens on
    // first read. A program that allocates (strings, vectors) keeps the
    // interface, and the Windows command line converts once at that point.
    let dir = unique_temp_dir("pay_as_you_go_allocator");
    let cli = env!("CARGO_BIN_EXE_ae");
    let scalar = dir.join("scalar.ae");
    fs::write(&scalar, "fn main() { print(6 * 7); print(2 - 9); }\n").unwrap();
    let emitted = Command::new(cli).arg("emit-c").arg(&scalar).output().unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let text = String::from_utf8_lossy(&emitted.stdout);
    for symbol in ["ae_os_alloc", "ae_allocator_impl", "ae_alloc", "ae_free", "ae_set_allocator", "malloc"] {
        assert!(!text.contains(symbol), "scalar program carries {symbol}");
    }
    let run = Command::new(cli).arg("run").arg(&scalar).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n-7\n");

    let stringy = dir.join("stringy.ae");
    fs::write(&stringy, "fn main() { print(\"hi\" + to_string(6 * 7)); }\n").unwrap();
    let emitted = Command::new(cli).arg("emit-c").arg(&stringy).output().unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let text = String::from_utf8_lossy(&emitted.stdout);
    assert!(text.contains("ae_allocator_impl") && text.contains("ae_alloc"), "{text}");
    let run = Command::new(cli).arg("run").arg(&stringy).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "hi42\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn windows_header_is_included_only_when_the_program_needs_it() {
    // A pure-computation program must not pay for parsing <windows.h>; a
    // program that reaches the file layer must still have every declaration.
    // Both generated units have to pass strict C11 syntax checking.
    let cc = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let cases = [
        ("examples/float_math.ae", false),
        ("examples/file_tidy.ae", true),
        ("examples/run_tool.ae", true),
    ];
    for (example, needs_header) in cases {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(example);
        let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&source).output().unwrap();
        assert!(generated.status.success(), "{example}: {}", String::from_utf8_lossy(&generated.stderr));
        let text = String::from_utf8(generated.stdout).unwrap();
        assert_eq!(text.contains("#include <windows.h>"), needs_header, "{example}: unexpected header decision");
        assert_eq!(text.contains("windows.h omitted"), !needs_header, "{example}: unexpected omission comment");
        let path = std::env::temp_dir().join(format!("ae-win-header-{}-{}.c", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, text).unwrap();
        let checked = Command::new(&cc).args(["-std=c11", "-pedantic-errors", "-Werror=return-type", "-fsyntax-only"]).arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(checked.status.success(), "{example}: {}", String::from_utf8_lossy(&checked.stderr));
    }
}
#[test]
fn forward_calls_and_mutual_recursion_work_in_both_build_modes() {
    let source = "fn main() { print(even(10)); print(odd(10)); } fn even(n: i64) -> i64 { if n == 0 { return 1; } else { return odd(n - 1); } } fn odd(n: i64) -> i64 { if n == 0 { return 0; } else { return even(n - 1); } }";
    let path = std::env::temp_dir().join(format!("ae-forward-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    for release in [false, true] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ae"));
        cmd.arg("run").arg(&path);
        if release { cmd.arg("--release"); }
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n0\n");
    }
    fs::remove_file(path).unwrap();
}
#[test]
fn forward_declarations_still_enforce_arity_and_unique_names() {
    for source in ["fn main() { print(f()); } fn f(x: i64) -> i64 { return x; }", "fn main() {} fn f() -> i64 { return 1; } fn f() -> i64 { return 2; }", "fn main() { if 1 == 1 { print(1); }"] {
        let path = std::env::temp_dir().join(format!("ae-forward-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn read_only_array_borrows_support_forwarding_and_bounds_checks() {
    let source = "fn first(a: &[i64; 3]) -> i64 { return a[0]; } fn sum(a: &[i64; 3]) -> i64 { return first(&a) + a[1] + a[2]; } fn main() { var data = [4, 5, 6]; print(sum(&data)); data[0] = 8; print(first(&data)); }";
    let path = std::env::temp_dir().join(format!("ae-borrow-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    for release in [false, true] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ae"));
        cmd.arg("run").arg(&path);
        if release { cmd.arg("--release"); }
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "15\n8\n");
    }
    fs::write(&path, "fn f(a: &[i64; 2]) -> i64 { return a[2]; } fn main() { let x = [1, 2]; print(f(&x)); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--release").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("array index out of bounds"));
    fs::remove_file(path).unwrap();
}
#[test]
fn invalid_borrows_are_rejected() {
    for source in [
        "fn f(a: &[i64; 2]) -> i64 { a[0] = 3; return 1; } fn main() {}",
        "fn f(a: &[i64; 2]) -> i64 { return a[0]; } fn main() { let a = [1]; print(f(&a)); }",
        "fn f(a: &[i64; 2]) -> i64 { return a[0]; } fn main() { let a = [1, 2]; print(f(a)); }",
        "fn f(a: &[i64; 2]) -> i64 { return a[0]; } fn main() { let x = 1; print(f(&x)); }",
        "fn f(a: &[i64; 0]) -> i64 { return 0; } fn main() {}",
        "fn f(a: &[i64; 2]) -> i64 { return a; } fn main() {}",
    ] {
        let path = std::env::temp_dir().join(format!("ae-borrow-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn modulo_and_negation_follow_documented_semantics() {
    let output = execute("fn main() { print(7 % 3); print((0 - 7) % 3); print(7 % (0 - 3)); print(-5 + 2); print(2 * -3); print(-(2 + 3)); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n-1\n1\n-3\n-6\n-5\n");
}
#[test]
fn modulo_and_negation_boundaries_fail_safely() {
    for (expr, expected) in [
        ("1 % 0", "division by zero"),
        ("(0 - 9223372036854775807 - 1) % (0 - 1)", "integer overflow"),
        ("-(0 - 9223372036854775807 - 1)", "integer overflow"),
    ] {
        let output = execute(&format!("fn main() {{ print({expr}); }}"));
        assert!(!output.status.success(), "{expr}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{expr}: {err}");
    }
}
#[test]
fn break_and_continue_control_loop_flow() {
    // i=1..11: odds accumulate until i>9 breaks; evens continue without accumulating.
    let source = "fn main() { var i = 0; var total = 0; while 1 == 1 { i = i + 1; if i % 2 == 0 { continue; } if i > 9 { break; } total = total + i; } print(total); print(i); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "25\n11\n");
    // `continue` must re-evaluate the loop condition, not skip it.
    let source = "fn main() { var n = 0; var hits = 0; while n < 5 { n = n + 1; if n == 3 { continue; } hits = hits + 1; } print(hits); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}
#[test]
fn break_and_continue_outside_loops_are_rejected() {
    for source in ["fn main() { break; }", "fn main() { continue; }", "fn main() { if 1 == 1 { break; } }", "fn f() -> i64 { continue; return 1; } fn main() {}"] {
        let path = std::env::temp_dir().join(format!("ae-loop-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn else_if_chains_evaluate_in_order() {
    let source = "fn classify(n: i64) -> i64 { if n < 0 { return 0 - 1; } else if n == 0 { return 0; } else if n < 10 { return 1; } else { return 2; } } fn main() { print(classify(0 - 5)); print(classify(0)); print(classify(5)); print(classify(50)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "-1\n0\n1\n2\n");
    // A chain without a final else may run no branch at all.
    let source = "fn main() { var x = 5; if x == 1 { print(1); } else if x == 2 { print(2); } print(9); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
}
#[test]
fn booleans_and_short_circuit_evaluation_are_correct() {
    let source = "fn show(x: i64) -> bool { print(x); return true; } fn main() { var hit = false; hit = show(1) && show(2); print(hit); hit = show(3) && false; print(hit); hit = false && show(4); print(hit); hit = true || show(5); print(hit); hit = show(6) || show(7); print(hit); hit = !hit; print(hit); let initial = false || show(8); hit = initial; hit = hit && show(9); print(hit); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // show(1), show(2) both run; show(3) runs and decides; false skips show(4);
    // true skips show(5); show(6) decides so show(7) is skipped.
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n2\ntrue\n3\nfalse\nfalse\ntrue\n6\ntrue\nfalse\n8\n9\ntrue\n");
}
#[test]
fn bool_functions_and_params_round_trip() {
    let source = "fn is_even(n: i64) -> bool { return n % 2 == 0; } fn xor(a: bool, b: bool) -> bool { if a && b { return false; } else { return a || b; } } fn main() { print(is_even(10)); print(is_even(7)); print(xor(true, false)); print(xor(true, true)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\nfalse\ntrue\nfalse\n");
}
#[test]
fn assert_passes_and_fails_at_the_right_location() {
    let output = execute("fn main() { assert(1 == 1); assert(2 > 1); print(9); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
    let output = execute("fn main() {\n  let x = 5;\n  assert(x == 6);\n}");
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("ae:3:3: assertion failed"), "{err}");
}
#[test]
fn print_handles_strings_with_escapes_and_unicode() {
    let output = execute("fn main() { print(\"hello, world\"); print(\"tab\\there\"); print(\"中文 😀\"); print(\"quote\\\"inside\"); print(len(\"a\\rb\")); print(\"back\\\\slash\"); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "hello, world\ntab\there\n中文 😀\nquote\"inside\n3\nback\\slash\n");
    // `\r` is three bytes only when it lands in the middle of a line: a lone
    // carriage return at the end of printed text would be swallowed by the
    // terminal, so the length assertion above is the real check.
    for (source, marker, code) in [
        (r#"fn main() { print("bad \q"); }"#, '\\', "E0002"),
        ("// \u{00e9}\nfn main() { print(\"open }", '"', "E0003"),
        ("fn main() { print(\"a\nb\"); }", '\n', "E0004"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-str-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae")).args(["check", "--json"]).arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!result.status.success(), "{source}");
        let diagnostic = String::from_utf8_lossy(&result.stdout);
        let start = source.find(marker).unwrap();
        let expected = format!("\"byte_start\":{start},\"byte_end\":{},\"code\":\"{code}\"", start + 1);
        assert!(diagnostic.contains(&expected), "{diagnostic}");
    }
}
#[test]
fn owned_strings_move_concatenate_and_release() {
    // print borrows; let/assign/arguments/returns move; + consumes both
    // operands; assignment re-initializes a moved variable.
    let source = "fn shout(name: string) -> string { return name + \"!\"; } fn main() { var greeting = \"hello\"; print(greeting); let loud = shout(greeting); print(loud); greeting = \"hi\"; print(greeting); let question = greeting + \" there\"; print(question); print(\"done\"); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "hello\nhello!\nhi\nhi there\ndone\n");
}
#[test]
fn moved_strings_are_rejected_at_compile_time() {
    for source in [
        "fn main() { let a = \"x\"; let b = a; print(a); }",
        "fn main() { let a = \"x\"; let b = a; let c = a; }",
        "fn main() { let a = \"x\"; f(a); print(a); } fn f(s: string) -> i64 { return 1; }",
        "fn main() { var s = \"x\"; s = s; }",
        "fn main() { let s = \"x\"; let t = s + s; }",
        "fn main() { let a = \"x\"; if true { let b = a; } print(a); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-move-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn strings_pass_to_functions_and_free_on_all_exit_paths() {
    // Early return, loop break, and scope exit must all release without
    // double-free; output stays correct.
    let source = "fn pick(n: i64) -> string { let a = \"a\"; let b = \"b\"; if n == 0 { return a + \"0\"; } var i = 0; while i < 3 { let tmp = \"t\"; if i == 2 { break; } i = i + 1; } return b + \"9\"; } fn main() { print(pick(0)); print(pick(1)); var s = \"x\"; if true { let inner = s + \"y\"; print(inner); } s = \"z\"; print(s); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a0\nb9\nxy\nz\n");
}
#[test]
fn build_cache_serves_identical_executables_and_rebuilds_on_corruption() {
    let dir = std::env::temp_dir().join(format!("ae-cache-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    let source = dir.join("prog.ae");
    fs::create_dir(&dir).unwrap();
    fs::write(&source, "fn main() { print(\"cached\"); }").unwrap();
    let executable = dir.join(if cfg!(windows) { "out.exe" } else { "out" });
    let build = |out: &std::path::Path| Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("build").arg(&source).arg("-o").arg(out)
        .env("AE_CACHE", &cache).env("AE_TIMINGS", "1").output().unwrap();
    // Cold build misses; a second build to a new path must hit the cache.
    assert!(build(&executable).status.success());
    let second = dir.join(if cfg!(windows) { "out2.exe" } else { "out2" });
    let hit = build(&second);
    assert!(hit.status.success(), "{}", String::from_utf8_lossy(&hit.stderr));
    let err = String::from_utf8_lossy(&hit.stderr);
    assert!(err.contains("cache: hit"), "expected cache hit, got: {err}");
    for exe in [&executable, &second] {
        let output = Command::new(exe).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "cached");
    }
    // Corrupting the cached entry must not poison future builds: the manifest
    // no longer describes the bytes on disk, so the entry is dropped and the
    // program is rebuilt instead of serving a dead executable.
    let entries = cache.join("entries");
    let entry = fs::read_dir(&entries).unwrap().next().unwrap().unwrap().path();
    let exe_name = if cfg!(windows) { "program.exe" } else { "program" };
    fs::write(entry.join(exe_name), b"garbage").unwrap();
    let third = dir.join(if cfg!(windows) { "out3.exe" } else { "out3" });
    let rebuilt = build(&third);
    assert!(rebuilt.status.success(), "{}", String::from_utf8_lossy(&rebuilt.stderr));
    assert!(String::from_utf8_lossy(&rebuilt.stderr).contains("cache: miss"), "{}", String::from_utf8_lossy(&rebuilt.stderr));
    let repaired = Command::new(&third).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&repaired.stdout).trim(), "cached");
    // A cache clear makes the next build miss and still succeed.
    let clear = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("clear")
        .env("AE_CACHE", &cache).output().unwrap();
    assert!(clear.status.success());
    let stats = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("stats")
        .env("AE_CACHE", &cache).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&stats.stdout).lines().next().unwrap_or(""), "entries: 0");
    let fourth = dir.join(if cfg!(windows) { "out4.exe" } else { "out4" });
    let miss = build(&fourth);
    assert!(miss.status.success(), "{}", String::from_utf8_lossy(&miss.stderr));
    assert!(String::from_utf8_lossy(&miss.stderr).contains("cache: miss"));
    let output = Command::new(&fourth).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "cached");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn frontend_cache_reuses_generated_c_until_an_imported_file_changes() {
    let dir = std::env::temp_dir().join(format!("ae-frontend-cache-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    let source = dir.join("prog.ae");
    let lib = dir.join("lib.ae");
    fs::create_dir(&dir).unwrap();
    fs::write(&lib, "pub fn answer() -> i64 { return 41; }").unwrap();
    fs::write(&source, "use \"lib.ae\";\nfn main() { print(answer() + 1); }").unwrap();
    let build = |out: &std::path::Path| Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("build").arg(&source).arg("-o").arg(out)
        .env("AE_CACHE", &cache).env("AE_TIMINGS", "1").output().unwrap();
    let stderr = |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).into_owned();

    // A cold build compiles the frontend and publishes the generated C.
    let first = dir.join(if cfg!(windows) { "one.exe" } else { "one" });
    let cold = build(&first);
    assert!(cold.status.success(), "{}", stderr(&cold));
    assert!(stderr(&cold).contains("frontend-cache: miss"), "{}", stderr(&cold));

    // Nothing moved, so the second build must not run the frontend at all,
    // and it must still produce the same executable.
    let second = dir.join(if cfg!(windows) { "two.exe" } else { "two" });
    let warm = build(&second);
    assert!(warm.status.success(), "{}", stderr(&warm));
    assert!(stderr(&warm).contains("frontend-cache: hit"), "{}", stderr(&warm));
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    let output = Command::new(&second).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");

    // Editing the imported file, not the entry file, must invalidate the entry:
    // the unit list is what the cache is keyed on.
    fs::write(&lib, "pub fn answer() -> i64 { return 42; }").unwrap();
    let third = dir.join(if cfg!(windows) { "three.exe" } else { "three" });
    let stale = build(&third);
    assert!(stale.status.success(), "{}", stderr(&stale));
    assert!(stderr(&stale).contains("frontend-cache: miss"), "{}", stderr(&stale));
    let output = Command::new(&third).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "43");

    // A damaged frontend entry is a miss, never a wrong binary.
    let frontend = cache.join("frontend");
    let entry = fs::read_dir(&frontend).unwrap().next().unwrap().unwrap().path();
    fs::write(entry.join("program.c"), b"garbage").unwrap();
    let fourth = dir.join(if cfg!(windows) { "four.exe" } else { "four" });
    let repaired = build(&fourth);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert!(stderr(&repaired).contains("frontend-cache: miss"), "{}", stderr(&repaired));
    let output = Command::new(&fourth).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "43");

    // The report counts the frontend entries, and clearing removes them too.
    let stats = || String::from_utf8_lossy(&Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("cache").arg("stats").env("AE_CACHE", &cache).output().unwrap().stdout).into_owned();
    assert!(stats().contains("frontend entries: 1"), "{}", stats());
    let clear = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("clear")
        .env("AE_CACHE", &cache).output().unwrap();
    assert!(clear.status.success());
    assert!(stats().contains("frontend entries: 0"), "{}", stats());
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn cache_and_version_commands_work() {
    let dir = std::env::temp_dir().join(format!("ae-cache-dir-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    let dir_out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("dir")
        .env("AE_CACHE", &cache).output().unwrap();
    assert!(dir_out.status.success());
    assert!(String::from_utf8_lossy(&dir_out.stdout).contains("cache"));
    let version = Command::new(env!("CARGO_BIN_EXE_ae")).arg("version").output().unwrap();
    assert!(version.status.success());
    let text = String::from_utf8_lossy(&version.stdout);
    // Track the manifest instead of a hard-coded number, so a version bump
    // (such as ADR-60's cache-key invalidation) cannot break this test.
    assert!(text.contains(concat!("ae ", env!("CARGO_PKG_VERSION"))), "{text}");
    let unknown = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("wat").output().unwrap();
    assert!(!unknown.status.success());
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn for_loops_iterate_ranges_with_break_and_continue() {
    // continue must still advance the counter; break exits early.
    let source = "fn main() { var total = 0; for i in 0..10 { if i % 2 == 0 { continue; } if i > 7 { break; } total = total + i; } print(total); for i in 3..3 { print(999); } for i in 0..3 { for j in 0..2 { total = total + 1; } } print(total); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // odds below 8: 1+3+5+7 = 16; empty range prints nothing; nested adds 6 → 22
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "16\n22\n");
}
#[test]
fn for_range_bounds_evaluate_once_in_order() {
    let source = "fn show(x: i64) -> i64 { print(x); return x; } fn main() { for i in show(1)..show(4) { print(i); } }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n4\n1\n2\n3\n");
}
#[test]
fn len_reports_array_lengths_and_string_sizes() {
    // Array len is compile-time; string len counts UTF-8 bytes at runtime.
    let source = "fn main() { let a = [5, 6, 7]; print(len(a)); let s = \"héllo\"; print(len(s)); print(len(s + \"!\")); var n = 0; for i in 0..len(a) { n = n + a[i]; } print(n); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // "héllo" = h + é (2 bytes) + l + l + o = 6 bytes; +1 for "!"
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n6\n7\n18\n");
}
#[test]
fn for_and_len_misuse_is_rejected() {
    for source in [
        "fn len(x: i64) -> i64 { return x; } fn main() {}",
        "fn main() { for i in 0..3 { i = 2; } }",
        "fn main() { for i in true..3 { print(i); } }",
        "fn main() { for i in 0..\"x\" { print(i); } }",
        "fn main() { for i in 0 { print(i); } }",
        "fn main() { let s = 1; print(len(s)); }",
        "fn main() { print(len(1)); }",
        "fn main() { let a = [1]; print(len(a + 1)); }",
        "fn main() { let a = \"x\"; let b = a; print(len(a)); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-for-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn for_loop_counter_overflow_fails_safely() {
    // The hidden increment is checked arithmetic: i reaching i64::MAX fails.
    let output = execute("fn main() { for i in 9223372036854775805..9223372036854775807 { print(i); } print(9223372036854775807); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let output = execute("fn main() { for i in 9223372036854775806..9223372036854775807 { print(i); } }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9223372036854775806");
}
#[test]
fn vectors_grow_push_pop_and_bounds_check() {
    // Growth crosses several realloc doublings; len tracks every change.
    let source = "fn main() { var v = vec[]; print(len(v)); for i in 0..100 { push(v, i * i); } print(len(v)); print(v[99]); pop(v); print(len(v)); print(v[98]); var w = vec[5, 6]; push(w, 7); print(len(w)); print(w[0] + w[1] + w[2]); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\n100\n9801\n99\n9604\n3\n18\n");
}
#[test]
fn vector_bounds_failures_report_locations() {
    // Write checks the index before evaluating the value (division by zero
    // must not fire first).
    for (source, expected) in [
        ("fn main() { var v = vec[1]; print(v[1]); }", "array index out of bounds"),
        ("fn main() { var v = vec[1]; print(v[0 - 1]); }", "array index out of bounds"),
        ("fn main() { var v = vec[1]; v[5] = 1 / 0; }", "array index out of bounds"),
        ("fn main() { var v = vec[]; pop(v); }", "pop from empty vector"),
    ] {
        let output = execute(source);
        assert!(!output.status.success(), "{source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}
#[test]
fn vectors_move_through_functions_and_release() {
    // Passing moves; returning moves out; loop break releases body vectors.
    let source = "fn sum(v: vec) -> i64 { var total = 0; for i in 0..len(v) { total = total + v[i]; } return total; } fn make(n: i64) -> vec { var v = vec[]; for i in 1..n + 1 { push(v, i); } return v; } fn main() { var a = make(4); print(len(a)); print(sum(a)); a = vec[9]; print(sum(a)); var b = make(3); if true { let inner = make(2); print(len(inner)); } print(len(b)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\n10\n9\n2\n3\n");
}
#[test]
fn vector_misuse_is_rejected_at_compile_time() {
    for source in [
        "fn main() { let v = vec[1]; push(v, 2); }",
        "fn main() { var v = vec[1]; let w = v; push(v, 2); }",
        "fn main() { var v = vec[1]; let w = v; print(len(v)); }",
        "fn main() { let v = vec[\"hi\", 2]; }",
        "fn main() { print(vec[1, 2]); }",
        "fn push(v: vec) -> i64 { return 1; } fn main() {}",
        "fn main() { var len = 1; }",
        "fn main() { var v = vec[1]; var w = vec[2]; v = w; push(v, 3); print(len(w)); }",
        "fn main() { var s = \"x\"; push(s, 1); }",
        "fn main() { var v = vec[1]; v = 5; }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-vec-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn moving_an_outer_value_inside_a_loop_is_rejected() {
    // The body runs again, so a value moved out of an outer variable would be
    // read after the move on the next iteration. Without a fixed point over the
    // body the compiler rejects the move instead of relying on the runtime
    // sentinel (ADR-20).
    for source in [
        "fn main() { var v = vec[1, 2]; while len(v) > 0 { print(v[0]); let w = v; } }",
        "fn main() { var s = \"x\"; var i = 0; while i < 2 { let t = s; print(len(t)); i = i + 1; } }",
        "fn main() { var s = \"x\"; for i in 0..2 { let t = s; print(len(t)); } }",
        "fn main() { let v = vec[1]; var s = \"x\"; for x in v { let t = s; print(len(t)); } }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-loop-move-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted a move inside a loop: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains("inside a loop body") && err.contains("re-initialize"), "{err}");
    }
    // Re-initializing before the body ends keeps the next iteration valid.
    let output = execute("fn main() { var s = \"x\"; var i = 0; while i < 2 { print(len(s)); s = \"yy\"; i = i + 1; } print(s); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n2\nyy\n");
}
#[test]
fn ownership_join_field_move_and_overwrite_release_are_enforced() {
    // M2.2: a move on one branch is dead at the merge point; a struct field
    // can never be moved out (docs/resource-model.md); overwriting an owned
    // local releases the old value, which emit-tree makes visible.
    let cases = [
        (concat!(
            "fn main() {\n",
            "    let s = \"x\";\n",
            "    if true { let t = s; print(t); }\n",
            "    print(s);\n",
            "}\n"
        ), "cannot use 's' because it was moved"),
        (concat!(
            "struct P { a: string, b: string }\n",
            "fn main() {\n",
            "    let p = P { a: \"x\", b: \"y\" };\n",
            "    let q = p.a;\n",
            "    print(q);\n",
            "    print(p.b);\n",
            "}\n"
        ), "cannot move a field out of a struct"),
    ];
    for (source, message) in cases {
        let path = std::env::temp_dir().join(format!(
            "ae-ownership-{}-{}.ae",
            std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(output.status.code(), Some(1), "{source}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(message), "{source}: {error}");
    }

    let output = execute(concat!(
        "fn main() {\n",
        "    var s = \"x\";\n",
        "    s = \"y\";\n",
        "    print(s);\n",
        "}\n"
    ));
    assert!(output.status.success(), "{}",
        String::from_utf8_lossy(&output.stderr));
    let tree_path = std::env::temp_dir().join(format!(
        "ae-drop-{}-{}.ae",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tree_path, concat!(
        "fn main() {\n",
        "    var s = \"x\";\n",
        "    s = \"y\";\n",
        "    print(s);\n",
        "}\n"
    )).unwrap();
    let tree = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-tree").arg(&tree_path).output().unwrap();
    fs::remove_file(tree_path).unwrap();
    assert!(tree.status.success(), "{}",
        String::from_utf8_lossy(&tree.stderr));
    let dumped = String::from_utf8_lossy(&tree.stdout);
    assert!(dumped.contains("(releases old)"), "{dumped}");
    assert_eq!(dumped.matches("release v0: string").count(), 1, "{dumped}");
}

#[test]
fn raw_pointer_access_goes_through_the_reviewed_macros() {
    // M2.4.03: the only unchecked memory accesses in generated C are the two
    // AE_PTR_* macros, so one grep lists every promise a program makes.
    let source = concat!(
        "fn main() {\n",
        "    var v = 41;\n",
        "    let p = &mut v as *mut i64;\n",
        "    unsafe { let _w = ptr_write(p, 42); print(p.deref()); }\n",
        "}\n"
    );
    let path = std::env::temp_dir().join(format!(
        "ae-ptr-macro-{}-{}.ae",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, source).unwrap();
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-c").arg(&path).output().unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(emitted.status.success(), "{}",
        String::from_utf8_lossy(&emitted.stderr));
    let c = String::from_utf8_lossy(&emitted.stdout);
    assert!(c.contains("#define AE_PTR_READ(place) (*(place))"), "{c}");
    assert!(c.contains("AE_PTR_WRITE(t"), "{c}");
    assert!(c.contains("AE_PTR_READ("), "{c}");
    assert!(run.status.success(), "{}",
        String::from_utf8_lossy(&run.stderr));
    let printed: Vec<String> = String::from_utf8_lossy(&run.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["42"]);
}

#[test]
fn concurrent_cache_writers_leave_one_usable_entry() {
    // M5.3.05: two builds share one cache directory and one key. Each writes
    // through its own staging name and renames C before the entry, so the
    // survivor is always a complete, matching pair.
    let dir = unique_temp_dir("cache-race");
    let source = dir.join("main.ae");
    fs::write(&source, "fn main() { print(\"race\"); }").unwrap();
    let cache = dir.join("cache");
    fs::create_dir(&cache).unwrap();
    let ae = env!("CARGO_BIN_EXE_ae");
    let build = |name: &str| {
        Command::new(ae)
            .arg("build").arg(&source)
            .arg("-o").arg(dir.join(name))
            .env("AE_CACHE", &cache)
            .output().unwrap()
    };
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| build("one.exe"));
        let b = scope.spawn(|| build("two.exe"));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(first.status.success(), "{}",
        String::from_utf8_lossy(&first.stderr));
    assert!(second.status.success(), "{}",
        String::from_utf8_lossy(&second.stderr));
    let entry = fs::read_dir(&cache).unwrap().count();
    assert!(entry >= 1, "the cache directory stayed empty");
    let rerun = build("three.exe");
    assert!(rerun.status.success(), "{}",
        String::from_utf8_lossy(&rerun.stderr));
    let output = Command::new(dir.join("three.exe")).output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "race");
}

#[test]
fn hosted_files_preserve_bytes_and_processes_recover_status() {
    // M3.2.03/04/06/07: bytes (NUL included) survive a copy, a non-ASCII path
    // round-trips, an empty file is Ok with length 0 while a missing one is a
    // typed Err, and a child receives an argument with spaces and reports its
    // own exit code.
    let dir = unique_temp_dir("hosted");
    let input = dir.join("nul_in.bin");
    let output = dir.join("nul_out.bin");
    fs::write(&input, b"a\0b\0c").unwrap();
    fs::write(dir.join("中文 名.txt"), "héllo").unwrap();
    fs::write(dir.join("empty.txt"), "").unwrap();
    let spawn = if cfg!(windows) {
        concat!(
            "    var argv = strvec_new();\n",
            "    push(argv, \"/c\");\n",
            "    push(argv, \"echo\");\n",
            "    push(argv, \"two words\");\n",
            "    let first = run_command(\"cmd\", argv);\n",
            "    var code = strvec_new();\n",
            "    push(code, \"/c\");\n",
            "    push(code, \"exit\");\n",
            "    push(code, \"7\");\n",
            "    let second = run_command(\"cmd\", code);\n"
        )
    } else {
        concat!(
            "    var argv = strvec_new();\n",
            "    push(argv, \"-c\");\n",
            "    push(argv, \"echo two words\");\n",
            "    let first = run_command(\"sh\", argv);\n",
            "    var code = strvec_new();\n",
            "    push(code, \"-c\");\n",
            "    push(code, \"exit 7\");\n",
            "    let second = run_command(\"sh\", code);\n"
        )
    };
    let source = format!(concat!(
        "fn main() {{\n",
        "    let parts = args();\n",
        "    let read = try_read_file(parts[1]);\n",
        "    match read {{\n",
        "        ReadResult::Ok(text) => {{\n",
        "            print(len(text));\n",
        "            let wrote = try_write_file(parts[2], text);\n",
        "            match wrote {{\n",
        "                WriteResult::Ok(n) => {{ print(n); }}\n",
        "                WriteResult::Err(reason) => {{ print(\"w \" + reason); }}\n",
        "            }}\n",
        "        }}\n",
        "        ReadResult::Err(reason) => {{ print(\"r \" + reason); }}\n",
        "    }}\n",
        "    let wide = try_read_file(parts[3]);\n",
        "    match wide {{\n",
        "        ReadResult::Ok(text) => {{ print(len(text)); print(text); }}\n",
        "        ReadResult::Err(reason) => {{ print(\"wide \" + reason); }}\n",
        "    }}\n",
        "    let empty = try_read_file(parts[4]);\n",
        "    match empty {{\n",
        "        ReadResult::Ok(text) => {{ print(len(text)); }}\n",
        "        ReadResult::Err(reason) => {{ print(\"empty \" + reason); }}\n",
        "    }}\n",
        "    let missing = try_read_file(parts[5]);\n",
        "    match missing {{\n",
        "        ReadResult::Ok(text) => {{ print(\"unexpected\"); }}\n",
        "        ReadResult::Err(reason) => {{ print(contains(reason, \"No such file\")); }}\n",
        "    }}\n",
        "{}",
        "    match first {{\n",
        "        CommandResult::Ok(finished) => {{ print(finished.status); print(contains(finished.output, \"two words\")); }}\n",
        "        CommandResult::Err(reason) => {{ print(\"spawn \" + reason); }}\n",
        "    }}\n",
        "    match second {{\n",
        "        CommandResult::Ok(finished) => {{ print(finished.status); }}\n",
        "        CommandResult::Err(reason) => {{ print(\"spawn \" + reason); }}\n",
        "    }}\n",
        "}}\n"
    ), spawn);
    let path = dir.join("main.ae");
    fs::write(&path, source).unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&path).arg("--")
        .arg(&input).arg(&output)
        .arg(dir.join("中文 名.txt")).arg(dir.join("empty.txt"))
        .arg(dir.join("no_such.txt")).output().unwrap();
    let copied = fs::read(&output).unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(run.status.success(), "{}",
        String::from_utf8_lossy(&run.stderr));
    assert_eq!(copied, b"a\0b\0c");
    let printed: Vec<String> = String::from_utf8_lossy(&run.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["5", "5", "6", "héllo", "0", "true", "0", "true", "7"]);
}

#[test]
fn library_layers_handle_growth_utf8_boundaries_and_map_resize() {
    // M3.1.03/04: a vector grows past every initial capacity and keeps every
    // element; drop elaboration emits one release for the binding.
    let growth = concat!(
        "fn main() {\n",
        "    var v = vec[1];\n",
        "    var i = 0;\n",
        "    while i < 40 { push(v, i * 2); i = i + 1; }\n",
        "    var total = 0;\n",
        "    for x in v { total = total + x; }\n",
        "    print(len(v));\n",
        "    print(total);\n",
        "}\n"
    );
    let output = execute(growth);
    assert!(output.status.success(), "{}",
        String::from_utf8_lossy(&output.stderr));
    let printed: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["41", "1561"]);
    let path = std::env::temp_dir().join(format!(
        "ae-layers-{}-{}.ae",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, growth).unwrap();
    let tree = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-tree").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(tree.status.success(), "{}",
        String::from_utf8_lossy(&tree.stderr));
    let dumped = String::from_utf8_lossy(&tree.stdout);
    assert_eq!(dumped.matches("release v0: vec").count(), 1, "{dumped}");

    // M3.1.05: byte length and code-point length disagree, a boundary slice
    // copies one character, and cutting inside one is a trap.
    let text = concat!(
        "fn main() {\n",
        "    let text = \"héllo世界\";\n",
        "    print(utf8_len(text));\n",
        "    print(len(text));\n",
        "    print(slice(text, 0, 1));\n",
        "}\n"
    );
    let output = execute(text);
    assert!(output.status.success(), "{}",
        String::from_utf8_lossy(&output.stderr));
    let printed: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["7", "12", "h"]);
    let split = concat!(
        "fn main() {\n",
        "    let text = \"héllo世界\";\n",
        "    print(slice(text, 0, 2));\n",
        "}\n"
    );
    let output = execute(split);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("string index splits a UTF-8 character"), "{error}");

    // M3.1.06: 100 keys force several resizes; removing the middle key keeps
    // the neighbours reachable.
    let map = concat!(
        "fn main() {\n",
        "    var m = map_new();\n",
        "    var i = 0;\n",
        "    while i < 100 { let key = \"k\" + to_string(i); m[key] = i; i = i + 1; }\n",
        "    let removed = map_remove(m, \"k50\");\n",
        "    print(len(m));\n",
        "    print(map_has(m, \"k50\"));\n",
        "    print(map_has(m, \"k99\"));\n",
        "}\n"
    );
    let output = execute(map);
    assert!(output.status.success(), "{}",
        String::from_utf8_lossy(&output.stderr));
    let printed: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["99", "false", "true"]);
}

#[test]
fn vector_growth_during_iteration_is_rejected() {
    // M2.3 gate: growth would reallocate the buffer the iteration loan
    // points at, so the write is refused while the loan is live.
    let output = execute(concat!(
        "fn main() {\n",
        "    var v = vec[1, 2];\n",
        "    for x in v { print(x); push(v, 3); }\n",
        "}\n"
    ));
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot modify 'v' while it is borrowed"),
        "{error}");
}

#[test]
fn returning_a_live_owned_local_keeps_the_buffer_valid() {
    // Returned slots transfer ownership and must survive the callee's drops.
    let source = r#"
        fn pick_v() -> vec { var v = vec[7, 8]; return v; }
        fn pick_s() -> string { let s = "live"; return s; }
        fn identity[T](value: T) -> T { return value; }
        fn main() {
            var v = pick_v();
            print(len(v));
            print(v[1]);
            print(pick_s());
            let floats = identity[vec[f64]](vec[1.5, 2.5]);
            print(floats[1]);
        }
    "#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\n8\nlive\n2.5\n");
}
#[test]
fn len_works_in_binary_and_condition_contexts() {
    // Regression: len() fast paths once skipped the binary-operator loop.
    let source = "fn main() { var v = vec[1, 2, 3]; let a = [9, 9, 9, 9]; let s = \"abcd\"; if len(v) > 2 { print(len(v) + 1); } print(len(a) * 2); print(len(s) == 4 && len(v) < len(a)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\n8\ntrue\n");
}
#[test]
fn deep_import_chain_loads_without_recursive_host_stack() {
    let dir = unique_temp_dir("deep-imports");
    const COUNT: usize = 700;
    for index in 0..COUNT {
        let name = format!("module_{index:04}.ae");
        let mut source = if index + 1 < COUNT {
            format!("use \"module_{:04}.ae\";\n", index + 1)
        } else { String::new() };
        if index == 0 { source.push_str("fn main() { print(7); }\n"); }
        fs::write(dir.join(name), source).unwrap();
    }
    let entry = dir.join("module_0000.ae");
    let library_entry = entry.clone();
    let library_result = std::thread::Builder::new().stack_size(512 * 1024)
        .spawn(move || ae_compiler::frontend::check_path(&library_entry)).unwrap()
        .join().expect("small-stack check must not panic");
    assert!(library_result.is_ok(), "{library_result:?}");
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&entry).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let deps = Command::new(env!("CARGO_BIN_EXE_ae")).arg("deps").arg(&entry).output().unwrap();
    assert!(deps.status.success(), "{}", String::from_utf8_lossy(&deps.stderr));
    let text = String::from_utf8_lossy(&deps.stdout);
    let modules: Vec<_> = text.lines().filter(|s| s.starts_with("module ")).collect();
    assert_eq!(modules.len(), COUNT);
    assert_eq!(modules.first().copied(), Some("module module_0699.ae"));
    assert_eq!(modules.last().copied(), Some("module module_0000.ae"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn diamond_and_repeated_imports_load_one_module_and_cycles_still_fail() {
    let dir = unique_temp_dir("diamond");
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("shared.ae"), "pub fn shared() -> i64 { return 7; }\n").unwrap();
    fs::write(dir.join("left.ae"), "use \"shared.ae\";\npub fn left() -> i64 { return shared(); }\n").unwrap();
    fs::write(dir.join("right.ae"), "use \"sub/../shared.ae\";\npub fn right() -> i64 { return shared() + 1; }\n").unwrap();
    let main = dir.join("main.ae");
    fs::write(&main, "use \"left.ae\";\nuse \"right.ae\";\nuse \"shared.ae\";\nfn main() { print(left()); print(right()); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&main).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n8\n");
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("deps").arg(&main).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().filter(|line| *line == "module shared.ae").count(), 1);

    fs::write(dir.join("shared.ae"), "use \"sub/bridge.ae\";\npub fn shared() -> i64 { return 7; }\n").unwrap();
    fs::write(dir.join("sub/bridge.ae"), "use \"../left.ae\";\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&main).output().unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("circular import: left.ae -> shared.ae -> sub/bridge.ae -> left.ae"), "{diagnostic}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn modules_import_transitively_and_reject_cycles_or_conflicts() {
    let dir = std::env::temp_dir().join(format!("ae-mod-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sub")).unwrap();
    // Cross-file names need `pub`; `hidden` stays private on purpose.
    fs::write(dir.join("lib.ae"), "use \"sub/deep.ae\";\npub fn twice(x: i64) -> i64 { return x * 2; }\npub fn quad(x: i64) -> i64 { return twice(twice(x)); }\nfn hidden() -> i64 { return 0; }\n").unwrap();
    fs::write(dir.join("sub/deep.ae"), "pub fn base() -> i64 { return 3; }\n").unwrap();
    let main_file = dir.join("main.ae");
    let executable = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let build_with = |text: &str, out: &std::path::Path| {
        fs::write(&main_file, text).unwrap();
        let _ = fs::remove_file(out);
        Command::new(env!("CARGO_BIN_EXE_ae")).arg("build").arg(&main_file).arg("-o").arg(out).output().unwrap()
    };
    // Transitive import: main -> lib -> sub/deep.
    let result = build_with("use \"lib.ae\";\nfn main() { print(base()); print(quad(base())); }", &executable);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let output = Command::new(&executable).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n12\n");
    // A name that exists but is not exported is rejected, not silently filtered.
    let result = build_with("use \"lib.ae\";\nfn main() { print(hidden()); }", &executable);
    assert!(!result.status.success());
    let err = String::from_utf8_lossy(&result.stderr).to_string();
    assert!(err.contains("'hidden' is private to") && err.contains("mark it 'pub'"), "{err}");
    // A private type cannot appear in an exported signature either.
    fs::write(dir.join("leaky.ae"), "struct Secret { n: i64 }\npub fn reveal(s: Secret) -> i64 { return s.n; }\n").unwrap();
    let result = build_with("use \"leaky.ae\";\nfn main() { print(1); }", &executable);
    assert!(!result.status.success());
    let err = String::from_utf8_lossy(&result.stderr).to_string();
    assert!(err.contains("exposes private struct 'Secret'"), "{err}");
    // Cycles are rejected with a diagnostic.
    fs::write(dir.join("cycle1.ae"), "use \"cycle2.ae\";\nfn a() -> i64 { return 1; }\nfn main() { print(a()); }").unwrap();
    fs::write(dir.join("cycle2.ae"), "use \"cycle1.ae\";\nfn b() -> i64 { return 2; }\n").unwrap();
    let result = build_with("use \"cycle1.ae\";\nfn main() { print(1); }", &executable);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("circular import"), "{}", String::from_utf8_lossy(&result.stderr));
    // Duplicate names across files are rejected.
    fs::write(dir.join("dupe.ae"), "fn twice(x: i64) -> i64 { return x; }\n").unwrap();
    let result = build_with("use \"lib.ae\";\nuse \"dupe.ae\";\nfn main() { print(1); }", &executable);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("duplicate function"), "{}", String::from_utf8_lossy(&result.stderr));
    // Missing import files fail cleanly; two mains are rejected.
    let result = build_with("use \"nope.ae\";\nfn main() { print(1); }", &executable);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot open"), "{}", String::from_utf8_lossy(&result.stderr));
    fs::write(dir.join("other.ae"), "fn main() { print(2); }").unwrap();
    let result = build_with("use \"other.ae\";\nfn main() { print(1); }", &executable);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("exactly one main"), "{}", String::from_utf8_lossy(&result.stderr));
    // Errors name the file and position.
    fs::write(dir.join("lib.ae"), "fn twice(x: i64) -> i64 { return x * 2; }\nfn quad(x: i64) -> i64 { return oops; }\n").unwrap();
    let result = build_with("use \"lib.ae\";\nfn main() { print(quad(1)); }", &executable);
    assert!(!result.status.success());
    let err = String::from_utf8_lossy(&result.stderr);
    assert!(err.contains("lib.ae") && err.contains("unknown variable"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn file_io_round_trips_unicode_paths_and_content() {
    let dir = std::env::temp_dir().join(format!("ae-io-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    // Non-ASCII file name exercises UTF-8 path handling end to end.
    let data_file = dir.join("数据-😀.txt");
    let data_path = data_file.display().to_string().replace('\\', "/");
    let literal = format!("\"{}\"", data_path.replace('\\', "\\\\"));
    let source = format!(
        "fn main() {{\n  let text = \"line one\\n中文行\\n\";\n  let n = write_file({lit}, text);\n  print(n);\n  let back = read_file({lit});\n  print(len(back));\n  print(back == text);\n}}",
        lit = literal
    );
    let path = std::env::temp_dir().join(format!("ae-io-src-{}.ae", std::process::id()));
    fs::write(&path, &source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{} / {}", String::from_utf8_lossy(&output.stderr), source);
    // "line one\n中文行\n" = 9 + 9 + 1 = 19 bytes (中文 = 6 bytes).
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "19\n19\ntrue\n");
    assert_eq!(fs::read_to_string(&data_file).unwrap(), "line one\n中文行\n");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn string_equality_compares_content() {
    let source = "fn main() { let a = \"hello\"; let b = \"hell\" + \"o\"; print(a == b); print(a != b); print(\"x\" == \"y\"); print(a == \"hello\"); print(read_file(\"z-无.txt\") == read_file(\"z-无.txt\")); }";
    let output = execute(source);
    assert!(!output.status.success()); // read of missing file fails at runtime — checks ordering below instead
    let source = "fn main() { let a = \"hello\"; let b = \"hell\" + \"o\"; print(a == b); print(a != b); print(\"x\" == \"y\"); print(a == \"hello\"); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\nfalse\nfalse\ntrue\n");
}
#[test]
fn file_io_failures_are_runtime_diagnostics() {
    let output = execute("fn main() { let text = read_file(\"definitely-missing-⟳.txt\"); print(len(text)); }");
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("cannot read file"), "{err}");
    let output = execute("fn main() { let n = write_file(\"/no/such/dir/x.txt\", \"a\"); print(n); }");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot write file"));
}
#[test]
fn bytes_builtin_converts_strings_for_indexing() {
    let source = "fn main() { let data = bytes(\"A中\"); print(len(data)); print(data[0]); print(data[1] * 256 + data[2]); var hits = 0; for i in 0..len(data) { if data[i] > 127 { hits = hits + 1; } } print(hits); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // "A中" = 65, 228, 184, 173 (4 bytes); 中 > 127 in every byte → 3 hits
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\n65\n58552\n3\n");
}
#[test]
fn file_builtins_are_reserved() {
    for source in ["fn read_file(p: string) -> string { return p; } fn main() {}", "fn main() { var bytes = 1; }", "fn write_file(a: string, b: string) -> i64 { return 1; } fn main() {}"] {
        let path = std::env::temp_dir().join(format!("ae-io-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn contains_and_slice_cover_text_processing() {
    let source = "fn main() { let t = \"the quick brown fox\"; print(contains(t, \"quick\")); print(contains(t, \"lazy\")); print(contains(t, \"\")); let word = slice(t, 4, 9); print(word); print(len(word)); print(word == \"quick\"); print(slice(t, 0, 3) + \" \" + slice(t, 16, 19)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\nfalse\ntrue\nquick\n5\ntrue\nthe fox\n");
    for (source, expected) in [
        ("fn main() { print(slice(\"abc\", 1, 5)); }", "string index out of bounds"),
        ("fn main() { print(slice(\"abc\", 2, 1)); }", "string index out of bounds"),
        ("fn main() { print(slice(\"abc\", 0 - 1, 2)); }", "string index out of bounds"),
    ] {
        let output = execute(source);
        assert!(!output.status.success(), "{source}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{source}");
    }
}
#[test]
fn for_in_iterates_vector_elements_by_value() {
    let source = "fn main() { var total = 0; let v = vec[3, 1, 4, 1, 5]; for x in v { total = total + x; } print(total); var hits = 0; for b in v { if b > 2 { hits = hits + 1; continue; } hits = hits + 100; } print(hits); var nested = 0; for x in v { for y in v { if x == y { nested = nested + 1; } } } print(nested); for x in v { if x == 4 { break; } total = total + 1000; } print(total); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // total=14; hits: 3,4,5 exceed 2 → +3; the two 1s add 100 each → 203;
    // equal pairs: 1 appears twice so (1,1) matches 4 times, plus (3,3)(4,4)(5,5) → 7;
    // break at x==4: x=3 and x=1 each add 1000 → total 14+2000=2014
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "14\n203\n7\n2014\n");
    for source in [
        "fn main() { let v = vec[1]; for x in v { x = 2; } }",
        "fn main() { let v = vec[1]; for x in v { let w = v; } }",
        "fn make(n: i64) -> vec { var v = vec[]; for i in 1..n + 1 { push(v, i); } return v; } fn main() { for x in make(3) { print(x); } }",
        "fn main() { for x in 5 { print(x); } }",
        "fn f(v: &vec) -> i64 { var t = 0; for x in v { t = t + x; } return t; } fn main() { print(f(&vec[1])); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-forin-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn structs_construct_access_and_move() {
    let source = "struct Point { x: i64, y: i64 } struct Named { id: i64, name: string } fn describe(n: Named) -> i64 { print(n.name); return n.id; } fn main() { var p = Point{x: 3, y: 4}; print(p.x + p.y); p.x = 30; print(p.x * p.y); let n = Named{id: 7, name: \"ae\"}; print(describe(n)); var q = Point{x: 1, y: 2}; q = Point{x: q.x + 10, y: q.y}; print(q.x + q.y); let s = \"tail\"; print(s == slice(\"head-tail\", 5, 9)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n120\nae\n7\n13\ntrue\n");
    for source in [
        "struct P { x: i64 } fn main() { let p = P{x: 1}; print(p.z); }",
        "struct P { x: i64 } fn main() { let p = P{x: 1, y: 2}; }",
        "struct P { x: i64 } fn main() { let p = P{y: 1}; }",
        "struct P { x: i64, s: string } fn main() { let p = P{x: 1, s: \"a\"}; let t = p.s; }",
        "struct P { x: i64 } fn main() { var p = P{x: 1}; p.x = true; }",
        "struct P { x: i64, p: P } fn main() { let q = P{x: 1}; }",
        "struct P { x: i64 } fn main() { let P = 1; }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-struct-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn struct_fields_release_on_all_exit_paths() {
    // Owned fields inside structs must release on scope exit, early return,
    // and break, without double-free (ASan on Linux verifies).
    let source = "struct Holder { tag: i64, data: string } fn pick(n: i64) -> i64 { var h = Holder{tag: n, data: \"payload\"}; if n == 0 { return h.tag; } for i in 0..2 { let tmp = Holder{tag: i, data: \"t\"}; if i == 1 { break; } } print(h.data); return h.tag; } fn main() { print(pick(0)); print(pick(5)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\npayload\n5\n");
}
#[test]
fn generic_structs_instantiate_one_layout_per_argument_list() {
    // The declaration is a shape; each argument list is a type. Two uses of the
    // same list share one layout (so they are interchangeable), and different
    // lists never mix.
    let source = r#"struct Pair[K, V] { left: K, right: V }
struct Tag { name: string, n: i64 }
fn read(p: Pair[i64, string]) -> i64 {
    print(p.right);
    return p.left;
}
fn main() {
    var p = Pair[string, i64] { left: "a", right: 1 };
    p.right = p.right + 1;
    print(p.left);
    print(p.right);
    let t = Pair[i64, Tag] { left: 5, right: Tag { name: "t", n: 9 } };
    print(t.left);
    print(t.right.name);
    let mixed = Pair[i64, string] { left: 7, right: "seven" };
    print(read(mixed));
    let nested = Pair[string, Pair[string, string]] { left: "x", right: Pair[string, string] { left: "y", right: "z" } };
    print(nested.right.left);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a\n2\n5\nt\nseven\n7\ny\n");
    for source in [
        // Too few type arguments, and a generic name without any.
        "struct P[T] { f: T } fn main() { let p = P[string, i64] { f: \"a\" }; }",
        "struct P[T] { f: T } fn main() { let p = P { f: \"a\" }; }",
        // A different argument list is a different type.
        "struct P[T] { f: T } fn take(p: P[string]) -> i64 { return 1; } fn main() { print(take(P[i64] { f: 2 })); }",
        // The field keeps its declared parameter type.
        "struct P[T] { f: T } fn main() { let p = P[string] { f: 1 }; }",
        // A generic declaration still owns its name.
        "struct P[T] { f: T } struct P { g: i64 } fn main() {}",
        "struct P[T] { f: T } enum P { A } fn main() {}",
        // No recursive layout: the name is not in scope inside its own body.
        "struct P[T] { next: P[T] } fn main() {}",
        // Type parameters are single uppercase letters.
        "struct P[a] { f: a } fn main() {}",
    ] {
        let path = std::env::temp_dir().join(format!("ae-generic-struct-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn generic_functions_are_templates_for_one_argument_list() {
    // The body is parsed once per argument list, so a `T` that is a string in
    // one instance is an i64 in another, and instantiating a generic function
    // from inside another generic body composes the substitution.
    let source = r#"fn identity[T](x: T) -> T {
    return x;
}
fn first[T](a: T, b: T) -> T {
    return a;
}
fn wrap[T](x: T) -> Option[T] {
    return Option[T]::Some(x);
}
fn double_wrap[T](x: T) -> Option[T] {
    let inner = wrap[T](x);
    return inner;
}
struct Pair[K, V] { left: K, right: V }
fn pair_of[T](x: T, n: i64) -> Pair[T, i64] {
    return Pair[T, i64] { left: x, right: n };
}
fn main() {
    print(identity[i64](41));
    print(identity[string]("ada"));
    print(first[string]("left", "right"));
    let found = double_wrap[string]("wrapped");
    match found {
        Option::Some(text) => { print(text); }
        Option::None => { print("none"); }
    }
    let pair = pair_of[string]("k", 2);
    print(pair.left);
    print(pair.right);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "41\nada\nleft\nwrapped\nk\n2\n");
    for (source, expected) in [
        // A parameter that occurs only in a concrete argument cannot be
        // inferred; the call still gets the explicit-type-argument remedy.
        ("fn id[T](x: i64) -> i64 { return x; } fn main() { print(id(5)); }", "is a generic function"),
        ("fn id[T](x: T) -> T { return x; } fn main() { print(id[i64, string](5)); }", "takes 1 type argument(s), found 2"),
        // A body is checked after substitution, against the concrete type.
        ("fn bad[T](x: T) -> i64 { return x + 1; } fn main() { print(bad[string](\"a\")); }", "'+' requires two strings"),
        // Instantiating itself with the same arguments would never finish.
        ("fn again[T](x: T) -> T { return again[T](x); } fn main() { print(again[i64](1)); }", "instantiated: again[i64] -> again[i64]"),
        // main cannot be generic, and a generic name is still one name.
        ("fn main[T]() -> i64 { return 1; }", "main cannot be generic"),
        ("fn id[T](x: T) -> T { return x; } fn id(x: i64) -> i64 { return x; } fn main() { print(1); }", "duplicate function 'id'"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-generic-fn-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}

#[test]
fn generic_calls_infer_direct_value_parameter_types() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("generic_inference.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "mode {label} failed: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (source, expected) in [
        // Every occurrence of one parameter must instantiate to the same type.
        ("fn first[T](a: T, b: T) -> T { return a; } fn main() { print(first(1, \"two\")); }", "argument 2 to 'first' must be i64"),
        // Concrete parameters are still checked after inferring the other ones.
        ("fn keep[T](value: T, index: i64) -> T { return value; } fn main() { print(keep(\"x\", \"index\")); }", "argument 2 to 'keep' must be i64"),
        // A return-only parameter has no value argument to infer from.
        ("fn make[T](x: i64) -> i64 { return x; } fn main() { print(make(1)); }", "write its type arguments"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-generic-infer-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}
#[test]
fn changing_the_working_directory_moves_relative_paths() {
    // set_cwd is a process-wide mutation: after it, relative paths resolve
    // against the new directory, and the previous one stays reachable only
    // because the test saved it.
    let root = std::env::temp_dir().join(format!("ae-cwd-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&root).unwrap();
    let inner = root.join("inner");
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let made = try_create_dir("{inner}");
    match made {{
        StatusResult::Ok => {{ print("created"); }}
        StatusResult::Err(reason) => {{ print("create failed"); }}
    }}
    let moved = set_cwd("{inner}");
    match moved {{
        StatusResult::Ok => {{ print("moved"); }}
        StatusResult::Err(reason) => {{ print("move failed"); }}
    }}
    let written = try_write_file("inside.txt", "data");
    match written {{
        WriteResult::Ok(count) => {{ print(count); }}
        WriteResult::Err(reason) => {{ print("write failed"); }}
    }}
    print(path_exists("inside.txt"));
    let back = set_cwd("{root}");
    match back {{
        StatusResult::Ok => {{ print("back"); }}
        StatusResult::Err(reason) => {{ print("back failed"); }}
    }}
    print(path_exists("inner/inside.txt"));
    let missing = set_cwd("{inner}/absent");
    match missing {{
        StatusResult::Ok => {{ print("unexpected success"); }}
        StatusResult::Err(reason) => {{ print("missing directory is an error"); }}
    }}
}}"#, inner = literal(&inner), root = literal(&root));
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "created\nmoved\n4\ntrue\nback\ntrue\nmissing directory is an error\n");
    fs::remove_dir_all(&root).unwrap();
}
#[test]
fn generic_constraints_reject_arguments_at_the_call_site() {
    // A bound is a shape check on the argument, not a proof about the body:
    // the body is still checked once per instantiation. The error therefore
    // points at the call site, with the allowed types spelled out.
    let source = r#"fn add[T: Add](a: T, b: T) -> T { return a + b; }
fn larger[T: Ord](a: T, b: T) -> T { if a > b { return a; } return b; }
fn same[T: Eq](a: T, b: T) -> bool { return a == b; }
fn copy_twice[T: Copy](x: T) -> T { let y = x; return y; }
fn both[T: Add + Eq](a: T, b: T) -> bool { return (a + b) == a; }
fn main() {
    print(add[i64](2, 3));
    print(add[string]("be", "ta"));
    print(larger[i64](4, 9));
    print(same[bool](true, true));
    print(same[string]("x", "x"));
    print(copy_twice[i64](7));
    print(both[i64](0, 5));
    print(true == true);
    print(true != false);
    print(false == true);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "5\nbeta\n9\ntrue\ntrue\n7\nfalse\ntrue\ntrue\nfalse\n");
    for source in [
        "fn add[T: Add](a: T, b: T) -> T { return a + b; } fn main() { print(add[bool](true, false)); }",
        "fn larger[T: Ord](a: T, b: T) -> T { if a > b { return a; } return b; } fn main() { print(larger[string](\"a\", \"b\")); }",
        "fn copy_twice[T: Copy](x: T) -> T { return x; } fn main() { print(copy_twice[string](\"a\")); }",
        "fn same[T: Eq](a: T, b: T) -> bool { return a == b; } fn main() { print(same[vec](vec[], vec[])); }",
        "fn f[T: Add + Eq](a: T) -> T { return a; } fn main() { print(f[bool](true)); }",
        "fn f[T: Printable](a: T) -> T { return a; } fn main() { print(f[i64](1)); }",
        "fn f[T: Add + Add](a: T) -> T { return a; } fn main() { print(f[i64](1)); }",
        "struct P[T: Copy] { x: T } fn main() { print(1); }",
        "enum E[T: Copy] { A(T), B } fn main() { print(1); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-bound-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}

#[test]
fn generic_body_capabilities_are_checked_at_the_declaration() {
    // ADR-40: Copy still admits bool, which cannot be added. The diagnostic
    // appears at the declaration even though the function is never called.
    let path = std::env::temp_dir().join(format!("ae-weak-bound-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn f[T: Copy](a: T, b: T) -> T { return a + b; }\nfn main() { print(1); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("declares T: Copy"), "{message}");
    assert!(message.contains("does not support it"), "{message}");
    assert!(message.contains(":1:5:"), "diagnostic must point at the constraint: {message}");

    // Narrowing the constraint to the exact supported set is accepted and runs
    // with each admitted type; no new runtime machinery is involved.
    let source = r#"fn f[T: Add](a: T, b: T) -> T { return a + b; }
fn main() { print(f[i64](2, 3)); print(f[string]("a", "b")); print(f[f64](1.5, 2.5)); }"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "5\nab\n4.0\n");
}

#[test]
fn many_generic_declarations_in_one_file_keep_their_own_bodies() {
    // ADR-41: sharing one token buffer per file must not move any declaration's
    // `start`; each instantiation still parses its own parameter list and body.
    let mut source = String::new();
    for i in 0..40 {
        source.push_str(&format!("fn f{i}[T: Add](a: T, b: T) -> T {{ return a + b; }}\n"));
        source.push_str(&format!("fn g{i}[T: Ord](a: T, b: T) -> T {{ if a > b {{ return a; }} return b; }}\n"));
    }
    source.push_str("fn main() { ");
    for i in 0..40 {
        source.push_str(&format!("print(f{i}[i64]({i}, 1)); print(g{i}[i64]({i}, 100)); "));
    }
    source.push('}');
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mut expected = Vec::new();
    for i in 0..40 {
        expected.push((i + 1).to_string());
        expected.push("100".to_owned());
    }
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), expected.join("\n") + "\n");
}

#[test]
fn capability_audit_applies_to_imported_generic_functions() {
    let dir = std::env::temp_dir().join(format!("ae-w4-import-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    let lib = dir.join("lib.ae");
    let main = dir.join("main.ae");
    fs::write(&main, "use \"lib.ae\";\nfn main() { print(combine[i64](1, 2)); }\n").unwrap();

    // The imported generic is audited in its own file: a weak constraint must
    // be rejected before the importer's call site is ever checked.
    fs::write(&lib, "pub fn combine[T: Eq](a: T, b: T) -> T { return a + b; }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&main).output().unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("declares T: Eq"), "{message}");
    assert!(message.contains("lib.ae"), "diagnostic must name the defining file: {message}");

    fs::write(&lib, "pub fn combine[T: Add](a: T, b: T) -> T { return a + b; }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&main).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn question_mark_propagates_none_as_well_as_err() {
    // An Option has the same two-variant shape as a result: None is returned
    // to the caller, Some binds its payload. A bare `call()?;` discards the
    // payload, which must still be released exactly once.
    let source = r#"fn maybe_find(flag: bool) -> Option[string] {
    if flag { return Option[string]::Some("found"); }
    return Option[string]::None();
}
fn upper(flag: bool) -> Option[string] { let text = maybe_find(flag)?; return Option[string]::Some(text + "!"); }
fn discard(flag: bool) -> Option[string] { maybe_find(flag)?; return Option[string]::Some("always"); }
fn maybe_numbers(flag: bool) -> Option[vec] {
    if flag { return Option[vec]::Some(vec[1, 2, 3]); }
    return Option[vec]::None();
}
fn count_numbers(flag: bool) -> Option[vec] { maybe_numbers(flag)?; return Option[vec]::Some(vec[9]); }
fn keep_numbers(flag: bool) -> Option[vec] { let items = maybe_numbers(flag)?; return Option[vec]::Some(items); }
enum Maybe { Some(string), None }
fn make_maybe() -> Maybe { return Maybe::Some("custom"); }
fn use_maybe() -> Maybe { let v = make_maybe()?; return Maybe::Some(v); }
fn main() {
    let a = upper(true);
    match a { Option::Some(s) => { print(s); } Option::None => { print("none"); } }
    let b = upper(false);
    match b { Option::Some(s) => { print(s); } Option::None => { print("none"); } }
    let c = discard(true);
    match c { Option::Some(s) => { print(s); } Option::None => { print("none"); } }
    let d = discard(false);
    match d { Option::Some(s) => { print(s); } Option::None => { print("none"); } }
    let e = count_numbers(true);
    match e { Option::Some(items) => { print(len(items)); } Option::None => { print("none"); } }
    let f = count_numbers(false);
    match f { Option::Some(items) => { print(len(items)); } Option::None => { print("none"); } }
    let g = keep_numbers(true);
    match g { Option::Some(items) => { print(len(items)); } Option::None => { print("none"); } }
    let h = keep_numbers(false);
    match h { Option::Some(items) => { print(len(items)); } Option::None => { print("none"); } }
    let m = use_maybe();
    match m { Maybe::Some(s) => { print(s); } Maybe::None => { print("none"); } }
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "found!\nnone\nalways\nnone\n1\nnone\n3\nnone\ncustom\n");
    for source in [
        "enum Weird { A(i64), B(i64) } fn f() -> Weird { return Weird::A(1); } fn g() -> Weird { let v = f()?; return Weird::A(v); } fn main() { print(1); }",
        "fn f() -> Option[i64] { return Option[i64]::None(); } fn main() { let v = f()?; print(v); }",
        "enum Maybe { Some(i64, i64), None } fn f() -> Maybe { return Maybe::Some(1, 2); } fn g() -> Maybe { let v = f()?; return Maybe::Some(v, 2); } fn main() { print(1); }",
        "enum Maybe { Some(i64), None(string) } fn f() -> Maybe { return Maybe::Some(1); } fn g() -> Maybe { let v = f()?; return Maybe::Some(v); } fn main() { print(1); }",
        "fn f() -> Option[string] { return Option[string]::None(); } fn g() -> Option[i64] { let v = f()?; return Option[i64]::Some(1); } fn main() { print(1); }",
        "fn f() -> Option[string] { return Option[string]::None(); } fn g() -> Option[string] { f(); return Option[string]::Some(\"x\"); } fn main() { print(1); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-option-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}

#[test]
fn split_and_args_produce_string_vectors() {
    let source = "fn main() { let parts = split(\"a,b,,c\", \",\"); print(len(parts)); print(parts[0]); print(len(parts[2])); let whole = split(\"hello\", \",\"); print(len(whole)); print(whole[0]); var empties = 0; for i in 0..len(parts) { if len(parts[i]) == 0 { empties = empties + 1; } } print(empties); print(join(parts, \"-\")); print(join(parts, \"\")); let a = args(); print(len(a) >= 1); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // join(parts, "-") = "a-b--c"; join(parts, "") = "abc"; args() includes at least argv[0]
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\na\n0\n1\nhello\n1\na-b--c\nabc\ntrue\n");
    let output = execute("fn main() { let v = split(\"x\", \"\"); print(len(v)); }");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("empty separator"));
    let output = execute("fn main() { let v = split(\"a,b\", \",\"); print(v[5]); }");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("array index out of bounds"));
}
#[test]
fn program_arguments_arrive_as_utf8_on_every_platform() {
    // Windows hands `main` the command line in the ANSI code page, so a path
    // typed on a Chinese system used to arrive as GBK bytes while every file API
    // expects UTF-8: the program could not open its own arguments. `args()` now
    // converts each argument through UTF-16 once (ADR-54). The temporary
    // directory name is ASCII but the subdirectory and the file are not, which is
    // exactly the shape that failed.
    let dir = unique_temp_dir("argv-utf8");
    let cjk = dir.join("中文目录");
    fs::create_dir(&cjk).unwrap();
    let target = cjk.join("note.txt");
    let source = dir.join("argv.ae");
    fs::write(&source, "fn main() { let p = args(); let n = write_file(p[1], \"ok\"); print(n); print(read_file(p[1])); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&source).arg("--").arg(&target)
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n").trim(), "2\nok");
    // The same text is a valid UTF-8 argument: three bytes per character.
    let source = dir.join("len.ae");
    fs::write(&source, "fn main() { let p = args(); print(len(p[1])); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&source).arg("--").arg("中文目录")
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n").trim(), "12");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn print_keeps_every_shape_in_order_with_non_ascii_text() {
    // ADR-55 moved every shape onto one sink so a console run (unbuffered) keeps
    // the order the program wrote. Through a pipe the bytes must be exactly the
    // UTF-8 the program produced, with one newline per value and nothing else.
    let source = r#"
fn main() {
    print("中文测试");
    let s = "混合 ascii 与 中文";
    print(s);
    print(42);
    print(true);
    print(2.5);
    print("引号 \" 与 制表符 \t 结束");
}
"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(text, "中文测试\n混合 ascii 与 中文\n42\ntrue\n2.5\n引号 \" 与 制表符 \t 结束\n");
    // A `\n` escape inside a literal is one newline on output and does not
    // truncate what follows it, now that every shape shares the same sink.
    let output = execute("fn main() { print(\"a\\nb\"); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a\nb\n");
}

#[test]
fn tokens_stream_the_same_boundaries_as_split_without_materialising() {
    // The lazy cursor must agree with split token for token, restart on a second
    // loop over the same binding, accept a struct field as its base, and keep
    // working with break/continue (which release body-local strings).
    let source = r#"
struct Rec { body: string }
fn mismatches(text: string, sep: string) -> i64 {
    let parts = split(text, sep);
    let cur = tokenize(text, sep);
    var i = 0;
    var bad = 0;
    for t in cur {
        if i >= len(parts) { bad = bad + 1; } else if parts[i] != t { bad = bad + 1; }
        i = i + 1;
    }
    if i != len(parts) { bad = bad + 1; }
    return bad;
}
fn main() {
    print(mismatches("alpha beta gamma", " "));
    print(mismatches("", " "));
    print(mismatches(" ", " "));
    print(mismatches("a b ", " "));
    print(mismatches("a  b", " "));
    print(mismatches("alpha::beta::::gamma", "::"));
    print(mismatches("no-separator", "::"));
    let line = "one two three four";
    let cur = tokenize(line, " ");
    var seen = 0;
    for t in cur {
        let copy = t + "!";
        if len(t) == 3 { continue; }
        if t == "four" { break; }
        seen = seen + len(copy);
    }
    print(seen);
    var again = 0;
    for t in cur { again = again + 1; }
    let rec = Rec { body: "p q r" };
    let fields = tokenize(rec.body, " ");
    var n = 0;
    for f in fields { n = n + 1; }
    print(again);
    print(n);
}
"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "0\n0\n0\n0\n0\n0\n0\n6\n4\n3\n"
    );
}

#[test]
fn token_cursor_nested_under_a_range_restarts_on_each_iteration() {
    let output = execute(r#"
fn main() {
    let text = "a b";
    for i in 0..2 {
        let cursor = tokenize(text, " ");
        for part in cursor { print(part); }
    }
}
"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a\nb\na\nb\n");
}

#[test]
fn tokens_reject_a_base_that_moves_is_written_or_is_a_temporary() {
    // The cursor borrows its base for the scope, so the base cannot be moved,
    // reassigned or have its field written while the cursor is alive; a
    // temporary makes no sense (it would be freed before the first iteration).
    let cases = [
        // The write case routes through the L1 adjudication (M2.5): the
        // cursor is still read after the write, so the rejection survives
        // with the NLL diagnostic.
        ("fn main() { var text = \"a b\"; let c = tokenize(text, \" \"); text = \"x\"; for t in c { print(t); } }", "may still be read"),
        ("fn main() { let text = \"a b\"; let c = tokenize(text, \" \"); let m = text; for t in c { print(t); } print(m); }", "may still be read"),
        ("struct R { body: string } fn main() { var r = R { body: \"a b\" }; let c = tokenize(r.body, \" \"); r.body = \"z\"; for t in c { print(t); } }", "may still be read"),
        ("fn main() { let text = \"a b\"; for t in tokenize(text, \" \") { print(t); } }", "bare cursor variable"),
        ("fn main() { let text = \"a b\"; let c = tokenize(text, \" \"); print(len(c)); }", "token cursor"),
        ("fn main() { let text = \"a b\"; let c = tokenize(text, \" \"); print(c); }", "token cursor"),
        ("fn main() { let c = tokenize(\"a b\", \" \"); for t in c { print(t); } }", "tokenize expects a string variable"),
    ];
    for (source, expected) in cases {
        let output = execute(source);
        assert!(!output.status.success(), "accepted: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
    // An empty separator is a runtime failure at the call site, like split.
    let output = execute("fn main() { let text = \"a b\"; let c = tokenize(text, \"\"); for t in c { print(t); } }");
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("empty separator"), "{err}");
    // The failure points at the call, not at 0:0 (the cursor carries its site).
    assert!(err.contains(":1:39"), "location missing: {err}");

    // The separator is copied into the cursor, so it has a fixed 16-byte
    // capacity: exactly 16 works, 17 is rejected rather than allocated.
    let output = execute("fn main() { let t = \"a0123456789abcdefb\"; let c = tokenize(t, \"0123456789abcdef\"); var n = 0; for x in c { print(x); n = n + 1; } print(n); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "a\nb\n2\n");
    let output = execute("fn main() { let t = \"aXb\"; let c = tokenize(t, \"0123456789abcdefX\"); for x in c { print(x); } }");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("longer than 16 bytes"));
}

#[test]
fn field_views_borrow_the_whole_struct_variable() {
    // Reading a string field through a view is allowed; the variable counts as
    // borrowed, so writes and moves are rejected while the view can be read.
    let output = execute("struct P { name: string } fn main() { let p = P { name: \"hello world\" }; let h = slice_view(p.name, 0, 5); print(h); print(len(p.name)); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "hello\n11\n");
    for (source, expected) in [
        ("struct P { name: string } fn main() { var p = P { name: \"hello\" }; let h = slice_view(p.name, 0, 2); p.name = \"replaced\"; print(h); }", "may still be read"),
        ("struct P { name: string } fn main() { var p = P { name: \"hello\" }; let h = slice_view(p.name, 0, 2); p = P { name: \"replaced\" }; print(h); }", "while it is borrowed"),
        // The move routes through the L1 adjudication: the view is read after
        // the move, so the rejection survives with the NLL diagnostic.
        ("struct P { name: string } fn main() { let p = P { name: \"hello\" }; let h = slice_view(p.name, 0, 2); let q = p; print(h); }", "may still be read"),
        ("struct I { name: string } struct O { inner: I } fn main() { let o = O { inner: I { name: \"x\" } }; let h = slice_view(o.inner.name, 0, 1); print(h); }", "only on a struct variable"),
        ("struct P { count: i64 } fn main() { let p = P { count: 1 }; let h = slice_view(p.count, 0, 1); print(h); }", "slice_view expects"),
    ] {
        let out = execute(source);
        assert!(!out.status.success(), "accepted: {source}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}
#[test]
fn fused_split_and_lines_keep_their_edge_cases() {
    // ADR-49 gives split/lines one fused arena. The scanning rules must not
    // change: left-to-right, non-overlapping multi-byte separators, empty
    // fields preserved, and no trailing empty line after a final newline.
    let output = execute("fn main() { let v = split(\"aaaa\", \"aa\"); print(len(v)); print(len(v[0])); print(len(v[2])); let w = split(\",a,\", \",\"); print(len(w)); print(w[0]); print(w[2]); let e = split(\"\", \",\"); print(len(e)); print(len(e[0])); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n0\n0\n3\n\n\n1\n0\n");
    let output = execute("fn main() { print(len(lines(\"\"))); print(len(lines(\"a\\n\"))); print(len(lines(\"\\n\"))); print(len(lines(\"a\\n\\nb\"))); let crlf = lines(\"a\\r\\nb\"); print(len(crlf)); print(len(crlf[0])); print(len(lines(\"a\"))); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\n1\n1\n3\n2\n2\n1\n");
    // Destroying arena elements (pop, store) and growing past the exact
    // capacity (push) must not free arena memory or leak the replacement.
    let output = execute("fn main() { var v = split(\"a b c d e\", \" \"); pop(v); print(len(v)); v[0] = \"zz\"; print(v[0]); push(v, \"tail\"); print(v[4]); print(join(v, \"|\")); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\nzz\ntail\nzz|b|c|d|tail\n");
    // An empty lines() result has exact capacity 0; pushing must grow the
    // pointer array instead of doubling zero bytes.
    let output = execute("fn main() { var v = lines(\"\"); print(len(v)); push(v, \"grown\"); print(len(v)); print(v[0]); var s = split(\"\", \",\"); push(s, \"b\"); print(len(s)); print(s[1]); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\n1\ngrown\n2\nb\n");
}
#[test]
fn strvec_elements_are_borrow_only_and_push_pop_work() {
    let source = "fn main() { var v = split(\"one,two\", \",\"); print(len(v)); push(v, \"three\"); print(len(v)); print(v[2]); pop(v); print(len(v)); v[0] = \"ONE\"; print(v[0]); print(contains(v[1], \"wo\")); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\n3\nthree\n2\nONE\ntrue\n");
    // A borrowed element may be concatenated: the result copies it and the
    // element stays valid and owned by the vector.
    let joined = execute("fn main() { let v = split(\"a\", \",\"); let s = v[0] + \"!\"; print(s); print(v[0]); }");
    assert!(joined.status.success(), "{}", String::from_utf8_lossy(&joined.stderr));
    assert_eq!(String::from_utf8_lossy(&joined.stdout).replace("\r\n", "\n"), "a!\na\n");
    for source in [
        "fn main() { var v = split(\"a\", \",\"); let w = v[0]; pop(v); print(len(w)); }",
        "fn main() { let v = split(\"a\", \",\"); let w = v[0]; let x = v; }",
        "fn main() { let v = split(\"a\", \",\"); print(v); }",
        "fn split(s: string, x: string) -> i64 { return 1; } fn main() {}",
        "fn args() -> i64 { return 1; } fn main() {}",
    ] {
        let path = std::env::temp_dir().join(format!("ae-sv-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn slice_views_are_zero_copy_and_borrow_tracked() {
    let source = "fn main() { let t = \"the quick brown fox\"; let q = slice_view(t, 4, 9); print(q); print(len(q)); print(contains(q, \"quick\")); print(q == \"quick\"); let head = slice_view(t, 0, 3); print(head); print(slice(head, 0, 2)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "quick\n5\ntrue\ntrue\nthe\nth\n");
    // Out-of-bounds views fail at runtime with the source location.
    let oob = execute("fn main() { let t = \"abc\"; let v = slice_view(t, 0, 9); print(len(v)); }");
    assert!(!oob.status.success());
    assert!(String::from_utf8_lossy(&oob.stderr).contains("string index out of bounds"));
    for source in [
        // moving or reassigning a borrowed base is an error
        "fn main() { let t = \"abc\"; let v = slice_view(t, 0, 1); let w = t; print(len(v)); }",
        "fn main() { var t = \"abc\"; let v = slice_view(t, 0, 1); t = \"x\"; print(len(v)); }",
        // slice_view needs a bare string variable
        "fn main() { let v = slice_view(\"abc\", 0, 1); print(len(v)); }",
        // a view cannot be moved into a function that owns its argument
        "fn take(s: string) -> i64 { return len(s); } fn main() { let t = \"abc\"; let v = slice_view(t, 0, 1); print(take(v)); }",
        // views cannot be returned (no lifetime tracking across functions)
        "fn grab() -> ? { let t = \"abc\"; return slice_view(t, 0, 1); } fn main() {}",
    ] {
        let path = std::env::temp_dir().join(format!("ae-view-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn for_in_iterates_string_vector_elements_as_views() {
    let source = "fn main() { let parts = split(\"alpha,beta,gamma\", \",\"); var total = 0; for w in parts { total = total + len(w); print(w); if contains(w, \"eta\") { print(1); } else { print(0); } } print(total); for w in parts { if w == \"beta\" { print(w); } } }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "alpha\n0\nbeta\n1\ngamma\n0\n14\nbeta\n");
    // The container cannot be modified or moved while iterated.
    for source in [
        "fn main() { let v = split(\"a,b\", \",\"); for w in v { push(v, \"x\"); } }",
        "fn main() { let v = split(\"a,b\", \",\"); for w in v { let held = v; } }",
        "fn main() { let v = split(\"a,b\", \",\"); for w in v { pop(v); } }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-forin-sv-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
#[test]
fn lines_splits_on_newlines_like_common_stdlibs() {
    let source = "fn main() { let l = lines(\"a\\nbb\\nccc\"); print(len(l)); print(l[0]); print(l[2]); let t = lines(\"x\\n\"); print(len(t)); let e = lines(\"\"); print(len(e)); let m = lines(\"\\n\\n\"); print(len(m)); let u = lines(\"中\\n文\"); print(u[0]); print(join(l, \"+\")); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\na\nccc\n1\n0\n2\n中\na+bb+ccc\n");
    // Trailing newline produces no final empty line; interior empties remain.
    let output = execute("fn main() { let l = lines(\"x\\n\\ny\"); print(len(l)); print(l[1]); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n\n");
    // lines() is reserved.
    let path = std::env::temp_dir().join(format!("ae-lines-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn lines(s: string) -> i64 { return 1; } fn main() {}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!output.status.success());
}
#[test]
fn count_lines_with_matches_reference_semantics() {
    // Reference: split into lines (trailing newline yields no final empty),
    // count lines whose content contains the pattern.
    let nl = "\n";
    let cases = [
        (format!("ae here{nl}nope{nl}ae again"), "ae", 2),
        (String::new(), "x", 0),
        ("one".to_string(), "two", 0),
        ("yes".to_string(), "", 1),
        (format!("x{nl}{nl}y"), "", 3),
        (format!("a{nl}b{nl}"), "b", 1),
        (format!("aaa{nl}aa{nl}a"), "aa", 2),
        (format!("{nl}{nl}"), "z", 0),
    ];
    fn ae_lit(t: &str) -> String {
        t.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
    }
    for (text, pat, expected) in cases {
        let src_text = ae_lit(&text);
        let src_pat = ae_lit(pat);
        let source = format!("fn main() {{ print(count_lines_with(\"{src_text}\", \"{src_pat}\")); }}");
        let output = execute(&source);
        assert!(output.status.success(), "{source}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected.to_string(), "{text:?} / {pat:?}");
    }
}
#[test]
fn cli_grep_reads_pattern_and_file_from_args() {
    let dir = std::env::temp_dir().join(format!("ae-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    let src = dir.join("grep_cli.ae");
    fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/grep_cli.ae"), &src).unwrap();
    let data = dir.join("data.txt");
    fs::write(&data, "hit one\nmiss\nhit two\n").unwrap();
    let executable = dir.join(if cfg!(windows) { "g.exe" } else { "g" });
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("build").arg(&src).arg("-o").arg(&executable).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let run = |args: &[&str]| Command::new(&executable).args(args).current_dir(&dir).output().unwrap();
    let hit = run(&["hit", "data.txt"]);
    assert!(hit.status.success());
    assert_eq!(String::from_utf8_lossy(&hit.stdout).replace("\r\n", "\n").trim(), "2");
    let miss = run(&["zzz", "data.txt"]);
    assert_eq!(String::from_utf8_lossy(&miss.stdout).replace("\r\n", "\n").trim(), "0");
    let usage = run(&[]);
    assert!(String::from_utf8_lossy(&usage.stdout).contains("usage"));
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn vec_borrows_are_read_only_and_forwardable() {
    // Borrowed vectors forward like array borrows; mutation and moves are rejected.
    let source = "fn total(v: &vec) -> i64 { var t = 0; for i in 0..len(v) { t = t + v[i]; } return t; } fn last_two(v: &vec) -> i64 { return v[len(v) - 2] + v[len(v) - 1]; } fn main() { var data = vec[1, 2, 3]; print(total(&data)); print(last_two(&data)); data[0] = 10; print(total(&data)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "6\n5\n15\n");
    for source in [
        "fn f(v: &vec) -> i64 { push(v, 1); return 0; } fn main() { var d = vec[1]; print(f(&d)); }",
        "fn f(v: &vec) -> i64 { v[0] = 5; return 0; } fn main() { var d = vec[1]; print(f(&d)); }",
        "fn f(v: &vec) -> i64 { let w = *v; return 0; } fn main() { var d = vec[1]; print(f(&d)); }",
        "fn f(v: &vec) -> i64 { return 0; } fn main() { print(f(vec[1])); }",
        "fn f(v: &vec) -> i64 { return 0; } fn main() { let x = 1; print(f(&x)); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-vecref-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "{source}");
    }
}
fn check_rejects(source: &str) -> String {
    let path = std::env::temp_dir().join(format!("ae-enum-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!output.status.success(), "accepted invalid program: {source}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}
#[test]
fn missing_field_name_has_the_same_diagnostic_in_both_parsers() {
    let expression = "point.()";
    let syntax = ae_syntax::expression::parse_operator_expression("main.ae", expression).unwrap_err();
    assert_eq!(syntax.message, "expected identifier");
    assert_eq!(syntax.code.as_deref(), Some("E0010"));
    let range = syntax.primary.unwrap().span.range;
    assert_eq!(&expression[range.clone()], "(");

    let source = format!(
        "struct Point {{ x: i64 }}\nfn main() {{ let point = Point {{ x: 7 }}; print({expression}); }}"
    );
    let offset = source.find(expression).unwrap();
    let dir = unique_temp_dir("missing-field-name");
    let path = dir.join("main.ae");
    fs::write(&path, &source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", "--json"]).arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "\"message\":\"expected identifier\",\"byte_start\":{},\"byte_end\":{},\"code\":\"E0010\"",
        offset + range.start, offset + range.end
    );
    assert!(json.contains(&expected), "{json}");
}

#[test]
fn malformed_variant_binding_has_the_same_span_in_both_parsers() {
    let pattern = "Pair::Value(first, )";
    let error = ae_syntax::pattern::parse_variant_pattern("main.ae", pattern).unwrap_err();
    assert_eq!(error.code.as_deref(), Some("E0010"));
    let range = error.primary.unwrap().span.range;

    let source = format!(
        "enum Pair {{ Value(i64, i64) }}\nfn main() {{ let pair = Pair::Value(4, 9); match pair {{ {pattern} => {{ print(first); }} }} }}"
    );
    let offset = source.find(pattern).unwrap();
    let dir = unique_temp_dir("match-pattern-span");
    let path = dir.join("main.ae");
    fs::write(&path, &source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", "--json"]).arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "\"byte_start\":{},\"byte_end\":{},\"code\":\"E0010\"",
        offset + range.start, offset + range.end
    );
    assert!(json.contains(&expected), "{json}");
    assert_eq!(&pattern[range], ")");
}

#[test]
fn pattern_parser_diagnostics_align_with_check_json() {
    // The compiler reads arm patterns through the shared syntax parser, so a
    // malformed pattern reports the same code and byte range through both
    // routes. The standalone string includes the trailing context the error
    // points at (an arrow, not EOF), keeping the ranges comparable.
    let cases = [
        ("Pair::Value(2, second)", Some("E0010")),
        ("Pair::Value(first =>", None),
    ];
    let ae = env!("CARGO_BIN_EXE_ae");
    for (pattern, code) in cases {
        let standalone = ae_syntax::pattern::parse_variant_pattern("main.ae", pattern).unwrap_err();
        assert_eq!(standalone.code.as_deref(), code, "{pattern:?}");
        let range = standalone.primary.as_ref().expect("primary label").span.range.clone();

        let source = format!(
            "enum Pair {{ Value(i64, i64), Zero }}\nfn main() {{ let pair = Pair::Value(4, 9); match pair {{ {pattern} {{ print(first); }} Pair::Zero => {{}} }} }}"
        );
        let offset = source.find(pattern).unwrap();
        let dir = unique_temp_dir("match-pattern-parity");
        let path = dir.join("main.ae");
        fs::write(&path, &source).unwrap();
        let output = Command::new(ae).args(["check", "--json"]).arg(&path).output().unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(output.status.code(), Some(1), "{pattern:?}");
        let json = String::from_utf8_lossy(&output.stdout);
        let code_json = match code {
            Some(code) => format!("\"code\":\"{code}\""),
            None => "\"code\":null".to_owned(),
        };
        let expected = format!(
            "\"byte_start\":{},\"byte_end\":{},{}",
            offset + range.start, offset + range.end, code_json
        );
        assert!(json.contains(&expected), "{pattern:?}: expected {expected:?} in {json}");
    }
}

#[test]
fn pattern_payload_shape_is_checked_against_the_declaration() {
    // The shared parser accepts the whole grammar surface; the declaration
    // decides which shapes are meaningful. Each rejection names the variant.
    let base = "enum Pair { Value(i64, i64), Zero }\nfn main() { let pair = Pair::Value(4, 9); match pair { {ARM} } }";
    let cases = [
        ("Pair::Value(Pair::Zero(x), second) => { print(second); }", "pattern payloads must be plain names"),
        ("Pair::Value(a, b, c) => { print(a); }", "binds 2 payload value(s)"),
        ("Pair::Value(a) => { print(a); }", "needs 2 binding(s)"),
        ("Pair::Value() => { print(1); }", "needs 2 binding(s)"),
        ("Pair::Zero(x) => { print(x); }", "has no payload"),
        ("Pair::Value(a, a) => { print(a); }", "duplicate variable 'a'"),
        ("Pair::Value(len, b) => { print(b); }", "is a built-in name and cannot be a pattern binding"),
    ];
    for (arm, expected) in cases {
        let source = base.replace("{ARM}", arm);
        let message = check_rejects(&source);
        assert!(message.contains(expected), "{arm}\n  expected '{expected}', got '{message}'");
    }
}

#[test]
fn standalone_closure_headers_match_compiler_execution() {
    use ae_syntax::expression::{parse_operator_expression, ExpressionKind};

    // The shared parser records the closure header: parameter order and
    // spellings, the return type, and the brace-inclusive body range.
    let literal = "|x: i64, y: i64| -> i64 { return x * 10 + y; }";
    let parsed = parse_operator_expression("main.ae", literal).unwrap();
    assert_eq!(parsed.range, 0..literal.len());
    let ExpressionKind::Closure { params, return_type, body } = parsed.kind else {
        panic!("expected a closure literal");
    };
    assert_eq!(params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["x", "y"]);
    assert_eq!(&literal[return_type.clone()], "i64");
    assert_eq!(&literal[body.clone()], "{ return x * 10 + y; }");

    // The compiler executes the same literals — a capture-inferred closure
    // and the empty-parameter shorthand — with the shared parser untouched.
    let source = r#"fn main() {
    let base = 10;
    let offset = |x: i64| -> i64 { return x + base; };
    print(offset(3));
    print(offset(4));
    let roll = || -> i64 { return 7; };
    print(roll());
    print(roll());
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "13\n14\n7\n7\n");
}

#[test]
fn enums_match_payloads_execute() {
    use ae_syntax::pattern::{parse_variant_pattern, PatternKind};

    let parsed = parse_variant_pattern("main.ae", "Shape::Rect(w, h)").unwrap();
    let PatternKind::Variant { payload: Some(bindings), .. } = parsed.kind else {
        panic!("expected a variant payload");
    };
    assert_eq!(bindings[0].kind, PatternKind::Binding("w".into()));
    assert_eq!(bindings[1].kind, PatternKind::Binding("h".into()));

    let source = "enum Shape { Circle(i64), Rect(i64, i64), Empty }
enum Msg { Text(string), Flag(bool) }
fn area(s: Shape) -> i64 { match s { Shape::Circle(r) => { return 3 * r * r; } Shape::Rect(w, h) => { return 10 * w + h; } Shape::Empty => { return 0; } } }
fn render(m: Msg) -> string { match m { Msg::Text(t) => { return \"t:\" + slice(t, 0, len(t)); } Msg::Flag(b) => { if b { return \"yes\"; } return \"no\"; } } }
fn main() {
    print(area(Shape::Circle(3)));
    print(area(Shape::Rect(4, 5)));
    print(area(Shape::Empty));
    let m = Msg::Text(\"hi\");
    print(render(m));
    print(render(Msg::Flag(false)));
}";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "27\n45\n0\nt:hi\nno\n");
}
#[test]
fn match_arms_do_not_capture_loop_control_flow() {
    // An arm body's break/continue must leave the enclosing loop, not the match.
    let source = "enum Signal { Stop, Skip, Go(i64) }
fn main() {
    var i = 0;
    var total = 0;
    while i < 10 {
        i = i + 1;
        var signal = Signal::Go(i);
        if i == 2 { signal = Signal::Skip; }
        if i == 4 { signal = Signal::Stop; }
        match signal {
            Signal::Stop => { break; }
            Signal::Skip => { continue; }
            Signal::Go(n) => { total = total + n; }
        }
    }
    print(i);
    print(total);
}";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // i stops at 4 (stop); total collects 1 + 3 = 4 (2 skipped, 4 breaks first).
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\n4\n");
}
#[test]
fn enum_values_move_and_assign_deterministically() {
    let source = "enum Slot { Empty, Text(string) }
fn describe(s: Slot) -> string { match s { Slot::Empty => { return \"empty\"; } Slot::Text(t) => { return \"text \" + slice(t, 0, len(t)); } } }
fn main() {
    var slot = Slot::Text(\"one\");
    slot = Slot::Text(\"two\");
    print(describe(slot));
    slot = Slot::Empty;
    print(describe(slot));
}";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "text two\nempty\n");
}
#[test]
fn invalid_enum_programs_are_rejected() {
    let cases = [
        ("enum E { A, B } fn main() { let x = E::A; match x { E::A => { print(1); } } }", "not exhaustive"),
        ("enum E { A, B } fn main() { let x = E::A; match x { E::A => { print(1); } E::A => { print(2); } E::B => { print(3); } } }", "matched twice"),
        ("enum E { A(i64) } fn main() { let x = E::A(1, 2); print(1); }", "payload"),
        ("enum E { A(i64) } fn main() { let x = E::A; print(1); }", "payload"),
        ("enum E { A(i64) } fn main() { let x = E::A(1); match x { E::A => { print(1); } } }", "binding"),
        ("enum E { A } fn main() { let x = E::A; match x { F::A => { print(1); } } }", "pattern must name"),
        ("enum E { A } enum E { B } fn main() { print(1); }", "duplicate enum"),
        ("enum E { A, A } fn main() { print(1); }", "duplicate variant"),
        ("enum E { A } fn main() { let E = 1; print(E); }", "names an enum"),
        ("enum E { A } fn main() { let x = 1; match x { E::A => { print(1); } } }", "bare enum variable"),
        ("enum E { A } fn main() { let x = E::A; let y = x; match x { E::A => { print(1); } } }", "moved"),
        ("enum E { A(string) } fn take(s: string) -> i64 { return len(s); } fn main() { let x = E::A(\"s\"); match x { E::A(t) => { print(take(t)); } } }", "must be string"),
    ];
    for (source, expected) in cases {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}\n  expected '{expected}', got '{message}'");
    }
}

#[test]
fn prelude_results_report_recoverable_errors() {
    let source = r#"fn main() {
    let parsed = parse_int("42");
    match parsed {
        IntResult::Ok(value) => { print(value + 1); }
        IntResult::Err(reason) => { print("bad"); }
    }
    let rejected = parse_int("12x");
    match rejected {
        IntResult::Ok(value) => { print(value); }
        IntResult::Err(reason) => { print(len(reason) > 0); }
    }
    let empty = parse_int("");
    match empty {
        IntResult::Ok(value) => { print(value); }
        IntResult::Err(reason) => { print("empty rejected"); }
    }
    let huge = parse_int("9223372036854775808");
    match huge {
        IntResult::Ok(value) => { print(value); }
        IntResult::Err(reason) => { print("overflow rejected"); }
    }
    let lowest = parse_int("-9223372036854775808");
    match lowest {
        IntResult::Ok(value) => { print(value + 1); }
        IntResult::Err(reason) => { print("bad"); }
    }
    print(to_string(0 - 1234));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "43\ntrue\nempty rejected\noverflow rejected\n-9223372036854775807\n-1234\n");
}
#[test]
fn fallible_file_io_reports_errors_as_values() {
    let dir = std::env::temp_dir().join(format!("ae-fallible-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let data = dir.join("data.txt");
    let summary = dir.join("summary.txt");
    let missing = dir.join("missing.txt");
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let opened = try_read_file("{data}");
    match opened {{
        ReadResult::Ok(text) => {{ print(len(text)); }}
        ReadResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let absent = try_read_file("{missing}");
    match absent {{
        ReadResult::Ok(text) => {{ print("unexpected success"); }}
        ReadResult::Err(reason) => {{ print(len(reason) > 0); }}
    }}
    let saved = try_write_file("{summary}", "ok");
    match saved {{
        WriteResult::Ok(written) => {{ print(written); }}
        WriteResult::Err(reason) => {{ print("write failed"); }}
    }}
}}"#, data = literal(&data), missing = literal(&missing), summary = literal(&summary));
    fs::write(&data, "hello").unwrap();
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "5\ntrue\n2\n");
    assert_eq!(fs::read_to_string(&summary).unwrap(), "ok");
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn concatenation_copies_borrowed_strings() {
    // A borrowed string (field, element, view) must be copied by `+` and stay
    // valid: handing it to str_cat directly would double-free its buffer.
    let source = r#"struct Label { text: string }
fn main() {
    let label = Label { text: "field" };
    print(label.text + "/tail");
    print(label.text);
    let base = "abcdef";
    let view = slice_view(base, 1, 4);
    print("[" + view + "]");
    print(view);
    let parts = split("a-b-c", "-");
    print(parts[0] + "!" + parts[2]);
    print(parts[1]);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "field/tail\nfield\n[bcd]\nbcd\na!c\nb\n");
}
#[test]
fn invalid_prelude_usage_is_rejected() {
    for source in [
        "fn main() { let r = try_read_file(1); print(1); }",
        "fn main() { let r = try_write_file(\"a\", 1); print(1); }",
        "fn main() { let r = parse_int(1); print(1); }",
        "fn main() { print(to_string(\"x\")); }",
        "fn main() { print(to_string(true)); }",
        "fn main() { let r = try_read_file(\"a\", \"b\"); print(1); }",
        "enum ReadResult { A } fn main() { print(1); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-prelude-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}

#[test]
fn directory_traversal_is_sorted_and_reports_errors() {
    let root = std::env::temp_dir().join(format!("ae-dir-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("nested/c.txt"), "c").unwrap();
    let missing = root.join("absent");
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let listed = try_list_dir("{root}");
    match listed {{
        DirResult::Ok(names) => {{
            print(len(names));
            for name in names {{
                print(name);
                print(path_is_dir("{root}/" + name));
            }}
        }}
        DirResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let absent = try_list_dir("{missing}");
    match absent {{
        DirResult::Ok(names) => {{ print("unexpected success"); }}
        DirResult::Err(reason) => {{ print(len(reason) > 0); }}
    }}
    print(path_exists("{root}/a.txt"));
    print(path_exists("{missing}"));
}}"#, root = literal(&root), missing = literal(&missing));
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // Entries are sorted by bytes: a.txt, b.txt, nested.
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\na.txt\nfalse\nb.txt\nfalse\nnested\ntrue\ntrue\ntrue\nfalse\n");
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn read_only_string_parameters_do_not_consume_their_arguments() {
    let source = r#"fn label(text: &string) -> string {
    return "[" + text + "]";
}
fn twice(text: &string) -> i64 {
    print(label(text));
    print(label(text));
    return len(text);
}
fn main() {
    let name = "hello";
    print(twice(name));
    print(name);
    print(twice("literal"));
    let base = "abcdef";
    let view = slice_view(base, 1, 3);
    print(twice(view));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "[hello]\n[hello]\n5\nhello\n[literal]\n[literal]\n7\n[bc]\n[bc]\n2\n");
}
#[test]
fn borrowed_string_vectors_can_be_iterated_and_read() {
    let source = r#"fn count(parts: &vec[string]) -> i64 {
    return len(parts);
}
fn longest(parts: &vec[string]) -> i64 {
    var best = 0;
    for part in parts {
        if len(part) > best { best = len(part); }
    }
    return best;
}
fn main() {
    let parts = split("aa,bbb,c", ",");
    print(count(&parts));
    print(longest(&parts));
    print(parts[1]);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n3\nbbb\n");
}
#[test]
fn invalid_filesystem_and_borrow_usage_is_rejected() {
    for source in [
        "fn main() { print(try_list_dir(1)); }",
        "fn main() { print(path_exists(1)); }",
        "fn main() { print(path_is_dir(1)); }",
        // a read-only parameter cannot be returned or handed on as owned
        "fn grab(s: &string) -> string { return s; } fn main() { print(len(grab(\"x\"))); }",
        "fn take(s: string) -> i64 { return len(s); } fn own(s: &string) -> i64 { return take(s); } fn main() { print(own(\"x\")); }",
        // a vec is not a string
        "fn f(s: &string) -> i64 { return len(s); } fn main() { let v = vec[1]; print(f(v)); }",
        // a borrowed string vector cannot be modified
        "fn f(parts: &vec[string]) -> i64 { push(parts, \"x\"); return 0; } fn main() { let p = split(\"a\", \",\"); print(f(&p)); }",
        "fn f(parts: &vec[string]) -> i64 { let q = parts; return 0; } fn main() { let p = split(\"a\", \",\"); print(f(&p)); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-fs-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
#[test]
fn path_algebra_joins_names_and_parents() {
    // Separator handling is uniform: '/' is emitted on both platforms so the
    // same program prints the same paths everywhere.
    let source = r#"fn main() {
    print(path_join("dir", "name.txt"));
    print(path_join("dir/", "name.txt"));
    print(path_join("dir", "/abs.txt"));
    print(path_join("", "name.txt"));
    print(path_join("dir", ""));
    print("[" + path_name("a/b/c.txt") + "]");
    print("[" + path_name("a/b/") + "]");
    print("[" + path_name("plain") + "]");
    print("[" + path_parent("a/b/c.txt") + "]");
    print("[" + path_parent("a/b") + "]");
    print("[" + path_parent("plain") + "]");
    print("[" + path_parent("/a") + "]");
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "dir/name.txt\ndir/name.txt\n/abs.txt\nname.txt\ndir\n[c.txt]\n[b]\n[plain]\n[a/b]\n[a]\n[]\n[/]\n"
    );
}
#[test]
fn file_metadata_reports_size_and_missing_paths() {
    let dir = std::env::temp_dir().join(format!("ae-meta-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let data = dir.join("data.txt");
    let missing = dir.join("missing.txt");
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let found = try_file_info("{data}");
    match found {{
        FileResult::Ok(meta) => {{
            print(meta.size);
            print(meta.is_dir);
            print(meta.mtime > 0);
        }}
        FileResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let folder = try_file_info("{dir}");
    match folder {{
        FileResult::Ok(meta) => {{ print(meta.is_dir); }}
        FileResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let absent = try_file_info("{missing}");
    match absent {{
        FileResult::Ok(meta) => {{ print("unexpected success"); }}
        FileResult::Err(reason) => {{ print(len(reason) > 0); }}
    }}
}}"#, data = literal(&data), dir = literal(&dir), missing = literal(&missing));
    fs::write(&data, "hello world").unwrap();
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "11\nfalse\ntrue\ntrue\ntrue\n");
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn file_operations_create_copy_rename_and_remove() {
    let dir = std::env::temp_dir().join(format!("ae-ops-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let work = dir.join("work");
    let nested = work.join("nested/deep");
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let made = try_create_dir("{nested}");
    match made {{
        StatusResult::Ok => {{ print("mkdir ok"); }}
        StatusResult::Err(reason) => {{ print("mkdir failed"); }}
    }}
    let again = try_create_dir("{nested}");
    match again {{
        StatusResult::Ok => {{ print("mkdir idempotent"); }}
        StatusResult::Err(reason) => {{ print("mkdir repeated failed"); }}
    }}
    let original = path_join("{nested}", "one.txt");
    assert(write_file(original, "payload") == 7);
    let copy = path_join("{nested}", "two.txt");
    let copied = try_copy_file(original, copy);
    match copied {{
        StatusResult::Ok => {{ print("copy ok"); }}
        StatusResult::Err(reason) => {{ print("copy failed"); }}
    }}
    let moved = path_join("{nested}", "three.txt");
    let renamed = try_rename_path(copy, moved);
    match renamed {{
        StatusResult::Ok => {{ print("rename ok"); }}
        StatusResult::Err(reason) => {{ print("rename failed"); }}
    }}
    // A directory is not a file: removal must refuse it instead of silently
    // deleting an empty tree on one platform only.
    let refused = try_remove_file("{work}");
    match refused {{
        StatusResult::Ok => {{ print("removed a directory"); }}
        StatusResult::Err(reason) => {{ print("directory refused"); }}
    }}
    let gone = try_remove_file(moved);
    match gone {{
        StatusResult::Ok => {{ print("remove ok"); }}
        StatusResult::Err(reason) => {{ print("remove failed"); }}
    }}
    let absent = try_remove_file(moved);
    match absent {{
        StatusResult::Ok => {{ print("removed twice"); }}
        StatusResult::Err(reason) => {{ print("missing reported"); }}
    }}
}}"#, nested = literal(&nested), work = literal(&work));
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "mkdir ok\nmkdir idempotent\ncopy ok\nrename ok\ndirectory refused\nremove ok\nmissing reported\n"
    );
    // The original survived, the copy moved to its new name, and the moved
    // file was really removed.
    assert_eq!(fs::read_to_string(nested.join("one.txt")).unwrap(), "payload");
    assert!(!nested.join("two.txt").exists());
    assert!(!nested.join("three.txt").exists());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn directory_removal_requires_an_empty_directory() {
    let dir = std::env::temp_dir().join(format!("ae-rmdir-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(dir.join("full")).unwrap();
    fs::create_dir_all(dir.join("empty")).unwrap();
    fs::write(dir.join("full/file.txt"), "x").unwrap();
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn main() {{
    let busy = try_remove_dir("{full}");
    match busy {{
        StatusResult::Ok => {{ print("removed a non-empty directory"); }}
        StatusResult::Err(reason) => {{ print("non-empty refused"); }}
    }}
    let empty = try_remove_dir("{empty}");
    match empty {{
        StatusResult::Ok => {{ print("empty removed"); }}
        StatusResult::Err(reason) => {{ print("empty failed"); }}
    }}
}}"#, full = literal(&dir.join("full")), empty = literal(&dir.join("empty")));
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "non-empty refused\nempty removed\n");
    assert!(dir.join("full/file.txt").exists());
    assert!(!dir.join("empty").exists());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn invalid_path_helpers_are_rejected() {
    for source in [
        "fn main() { print(path_join(1, \"b\")); }",
        "fn main() { print(path_name(1)); }",
        "fn main() { print(path_parent(1)); }",
        "fn main() { let x = try_file_info(1); print(1); }",
        "fn main() { let x = try_copy_file(\"a\", 1); print(1); }",
        "fn main() { let x = try_rename_path(1, \"b\"); print(1); }",
        "fn main() { let x = try_remove_file(1); print(1); }",
        "fn main() { let x = try_create_dir(1); print(1); }",
        "fn main() { let x = try_remove_dir(1); print(1); }",
        "fn main() { let path_join = 1; }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-path-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
#[test]
fn run_forwards_program_arguments_after_double_dash() {
    let dir = std::env::temp_dir().join(format!("ae-argv-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    let source = dir.join("argv.ae");
    fs::write(&source, "fn main() { let a = args(); print(len(a) - 1); print(a[1]); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&source).arg("--").arg("first").arg("second")
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\nfirst\n");
    // Only `run` starts a program, so passing arguments to another command is
    // an error rather than silently ignored.
    let rejected = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg(&source).arg("--").arg("extra")
        .output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("does not take program arguments"));
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn error_propagation_returns_the_first_failure() {
    let dir = std::env::temp_dir().join(format!("ae-propagate-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("a.txt"), "alpha").unwrap();
    fs::write(dir.join("b.txt"), "beta").unwrap();
    let literal = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let source = format!(r#"fn merge(first: &string, second: &string) -> ReadResult {{
    let a = try_read_file(first)?;
    let b = try_read_file(second)?;
    return ReadResult::Ok(a + b);
}}

fn double(text: &string) -> IntResult {{
    let n = parse_int(text)?;
    return IntResult::Ok(n * 2);
}}

fn remove_missing(path: &string) -> StatusResult {{
    try_remove_file(path)?;
    return StatusResult::Ok;
}}

fn main() {{
    let both = merge("{a}", "{b}");
    match both {{
        ReadResult::Ok(text) => {{ print(text); }}
        ReadResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let half = merge("{a}", "{missing}");
    match half {{
        ReadResult::Ok(text) => {{ print("unexpected success"); }}
        ReadResult::Err(reason) => {{ print(len(reason) > 0); }}
    }}
    let doubled = double("21");
    match doubled {{
        IntResult::Ok(n) => {{ print(n); }}
        IntResult::Err(reason) => {{ print("unexpected failure"); }}
    }}
    let rejected = double("21x");
    match rejected {{
        IntResult::Ok(n) => {{ print("unexpected success"); }}
        IntResult::Err(reason) => {{ print(reason); }}
    }}
    let absent = remove_missing("{missing2}");
    match absent {{
        StatusResult::Ok => {{ print("unexpected success"); }}
        StatusResult::Err(reason) => {{ print(len(reason) > 0); }}
    }}
}}"#, a = literal(&dir.join("a.txt")), b = literal(&dir.join("b.txt")),
        missing = literal(&dir.join("missing.txt")), missing2 = literal(&dir.join("gone.txt")));
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "alphabeta\ntrue\n42\nnot a base-10 integer\ntrue\n"
    );
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn invalid_error_propagation_is_rejected() {
    for source in [
        // '?' needs a function that returns the same result type
        "fn main() { let a = try_read_file(\"x\")?; print(1); }",
        "fn f(p: &string) -> StatusResult { let names = try_list_dir(p)?; return StatusResult::Ok; } fn main() { print(1); }",
        // the value has to be a result at all
        "fn f() -> ReadResult { let x = 1?; return ReadResult::Ok(\"a\"); } fn main() { print(1); }",
        // only the prelude-shaped Ok/Err pairs can be propagated
        "enum Pair { Ok(string), Bad(string) } fn f() -> Pair { let x = try_read_file(\"a\")?; return Pair::Ok(x); } fn main() { print(1); }",
        // a bare call statement must state what happens to the result
        "fn f(p: &string) -> StatusResult { try_remove_file(p); return StatusResult::Ok; } fn main() { print(1); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-propagate-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
#[test]
fn float_maps_store_get_and_report_keys_like_int_maps() {
    // ADR-65: map[string, f64] mirrors map[string, i64] — put/index-write,
    // map_get into FloatResult (missing is the shared "key not found" text),
    // has/remove/keys/len and map_inc with a float delta.
    let source = "fn main() { var m = f64map_new(); m[\"pi\"] = 3.5; print(len(m)); let got = map_get(m, \"pi\"); match got { FloatResult::Ok(v) => { print(v); } FloatResult::Err(reason) => { print(reason); } } print(map_has(m, \"pi\")); print(map_has(m, \"nope\")); print(map_inc(m, \"pi\", 0.5)); print(map_inc(m, \"fresh\", 1.0)); m[\"zed\"] = 2.0; let ks = keys(m); for k in ks { print(k); } let removed = map_remove(m, \"fresh\"); print(removed); print(len(m)); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "1\n3.5\ntrue\nfalse\n4.0\n1.0\nfresh\npi\nzed\ntrue\n2\n");
}
#[test]
fn float_maps_reject_wrong_value_types_and_move_like_int_maps() {
    for (source, expected) in [
        ("fn main() { var m = f64map_new(); m[\"a\"] = 1; }", "map values must be f64"),
        ("fn main() { var m = map_new(); m[\"a\"] = 1.0; }", "map values must be i64"),
        ("fn main() { var m = f64map_new(); print(map_inc(m, \"a\", 1)); }", "map_inc expects a f64 delta"),
        ("fn main() { var m = f64map_new(); let g = map_get(m, \"a\"); match g { IntResult::Ok(v) => { print(v); } IntResult::Err(r) => { print(r); } } }", "pattern must name the matched enum"),
        ("fn main() { var m = f64map_new(); let w = m; print(map_has(m, \"a\")); }", "moved"),
    ] {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}: {message}");
    }
    // A `map[string, f64]` initializer spelling is rejected by both routes with
    // different wording (name-aware "unknown variable" vs the shared grammar's
    // bracket diagnostic); only the rejection is contractual.
    check_rejects("fn main() { var m = map[string, f64]; }");
}
#[test]
fn float_maps_live_in_structs_and_release_exactly_once() {
    let source = "struct Prices { table: map[string, f64], name: string } fn main() { var p = Prices { table: f64map_new(), name: \"menu\" }; p.table[\"tea\"] = 2.5; p.table[\"cake\"] = 7.0; print(p.name); print(len(p.table)); let t = map_get(p.table, \"tea\"); match t { FloatResult::Ok(v) => { print(v); } FloatResult::Err(r) => { print(r); } } print(\"done\"); }";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "menu\n2\n2.5\ndone\n");
}
#[test]
fn maps_store_count_replace_and_report_keys_in_order() {
    let output = execute(r#"fn main() {
    var counts = map_new();
    counts["b"] = 2;
    counts["a"] = 1;
    counts["b"] = 5;
    print(len(counts));
    print(map_has(counts, "a"));
    print(map_has(counts, "zz"));
    let got = map_get(counts, "b");
    match got { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print("unexpected failure"); } }
    let absent = map_get(counts, "zz");
    match absent { IntResult::Ok(n) => { print("unexpected success"); } IntResult::Err(reason) => { print(reason); } }
    let names = keys(counts);
    for name in names { print(name); }
    print(map_remove(counts, "a"));
    print(map_remove(counts, "a"));
    print(len(counts));
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "2\ntrue\nfalse\n5\nkey not found\na\nb\ntrue\nfalse\n1\n"
    );
}
#[test]
fn missing_map_key_text_survives_every_owned_string_path() {
    // ADR-58: a miss hands out fixed text in read-only data instead of a fresh
    // allocation. Match binds the payload as a view, so the owned holder is
    // the result value itself: this exercises reads, concatenation on either
    // side, copying into a string vector, moving the whole result into a
    // function, and `?` propagation across a function boundary.
    let output = execute(r#"fn consume(r: IntResult) -> i64 {
    match r { IntResult::Ok(n) => { return n; } IntResult::Err(e) => { return len(e); } }
}
fn propagated() -> IntResult {
    var m = map_new();
    let n = map_get(m, "absent")?;
    return IntResult::Ok(n);
}
fn main() {
    var m = map_new();
    m["present"] = 7;
    for i in 0..2000 {
        let r = map_get(m, "absent");
        match r {
            IntResult::Ok(n) => { print("unexpected"); }
            IntResult::Err(e) => {
                if i == 0 { print(e); print(len(e)); print(e == "key not found"); print(e); }
            }
        }
    }
    let left = map_get(m, "absent");
    match left { IntResult::Ok(n) => { print("unexpected"); } IntResult::Err(e) => { print("prefix: " + e); } }
    let right = map_get(m, "absent");
    match right { IntResult::Ok(n) => { print("unexpected"); } IntResult::Err(e) => { print(e + " suffix"); } }
    let moved = map_get(m, "absent");
    print(consume(moved));
    let collected = map_get(m, "absent");
    match collected {
        IntResult::Ok(n) => { print("unexpected"); }
        IntResult::Err(e) => { var notes = strvec_new(); push(notes, e); print(notes[0]); print(len(notes)); }
    }
    let p = propagated();
    match p { IntResult::Ok(n) => { print("unexpected"); } IntResult::Err(e) => { print(e); } }
    let hit = map_get(m, "present");
    match hit { IntResult::Ok(n) => { print(n); } IntResult::Err(e) => { print("unexpected"); } }
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "key not found\n13\ntrue\nkey not found\nprefix: key not found\nkey not found suffix\n13\nkey not found\n1\nkey not found\n7\n"
    );
}
#[test]
fn keys_after_removal_skips_tombstones_and_sees_reused_slots() {
    // The state byte decides what keys() reports, so a tombstone must be invisible
    // and a slot reused by a later insert must show up exactly once (ADR-57 moved
    // the state out of the slot record; this pins the observable consequence).
    let output = execute(r#"fn main() {
    var m = map_new();
    m["a"] = 1;
    m["b"] = 2;
    m["c"] = 3;
    print(map_remove(m, "b"));
    print(len(m));
    let first = keys(m);
    for k in first { print(k); }
    m["d"] = 4;
    m["b"] = 5;
    let second = keys(m);
    for k in second { print(k); }
    print(len(m));
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "true\n2\na\nc\na\nb\nc\nd\n4\n"
    );
}

#[test]
fn maps_survive_storage_in_struct_fields() {
    let output = execute(r#"struct Tally { label: string, counts: map[string, i64] }
fn record(label: &string) -> Tally {
    var counts = map_new();
    counts["seen"] = 1;
    counts["seen"] = 2;
    return Tally { label: slice(label, 0, len(label)), counts: counts };
}
fn main() {
    let t = record("day");
    print(t.label);
    print(len(t.counts));
    print(map_has(t.counts, "seen"));
    let got = map_get(t.counts, "seen");
    match got { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print("unexpected failure"); } }
    let names = keys(t.counts);
    print(names[0]);
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "day\n1\ntrue\n2\nseen\n");
}
#[test]
fn maps_reject_wrong_keys_values_and_use_after_move() {
    for source in [
        "fn main() { var m = map_new(); m[1] = 2; }",
        "fn main() { var m = map_new(); m[\"a\"] = \"b\"; }",
        "fn main() { let m = map_new(); m[\"a\"] = 1; }",
        "fn main() { var m = map_new(); print(map_get(1, \"a\")); }",
        "fn main() { var m = map_new(); print(map_has(m, 2)); }",
        "fn main() { var m = map_new(); m[\"a\"] = 1; print(m[\"a\"]); }",
        "fn take(m: map[string, i64]) -> i64 { return len(m); } fn main() { var m = map_new(); print(take(m)); print(len(m)); }",
        "fn main() { var a: map[string, bool] = map_new(); print(len(a)); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-map-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
fn execute_with_env(source: &str, env: &[(&str, &str)]) -> std::process::Output {
    let path = std::env::temp_dir().join(format!("ae-integration-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
    command.arg("run").arg(&path);
    for (key, value) in env { command.env(key, value); }
    let output = command.output().unwrap();
    fs::remove_file(path).unwrap();
    output
}
#[test]
fn utf8_length_and_prefix_count_characters_not_bytes() {
    let output = execute(r#"fn main() {
    let text = "中文 😀 abc";
    print(len(text));
    print(utf8_len(text));
    print(utf8_prefix(text, 2));
    print(utf8_prefix(text, 3));
    print(utf8_prefix(text, 100) == text);
    print(utf8_prefix("", 3) == "");
    print(utf8_prefix("abc", 0) == "");
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "15\n8\n中文\n中文 \ntrue\ntrue\ntrue\n"
    );
}
#[test]
fn slicing_inside_a_character_is_rejected() {
    for source in [
        "fn main() { let text = \"中文\"; print(slice(text, 0, 1)); }",
        "fn main() { let text = \"中文\"; print(slice(text, 3, 5)); }",
        "fn main() { let text = \"中文\"; print(slice_view(text, 1, 3)); }",
        // A whole-character range is fine, and so is the empty range at a boundary.
        "fn main() { let text = \"中文\"; print(slice(text, 3, 6)); print(utf8_len(slice(text, 0, 0))); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-utf8-slice-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        let allowed = source.contains("utf8_len(slice(text, 0, 0))");
        if allowed {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "文\n0\n");
        } else {
            assert!(!output.status.success(), "accepted a slice that splits a character: {source}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("string index splits a UTF-8 character"), "{}", String::from_utf8_lossy(&output.stderr));
        }
    }
}
#[test]
fn split_accepts_borrowed_separators() {
    let output = execute(r#"fn fields(text: &string, sep: &string) -> i64 {
    let parts = split(text, sep);
    return len(parts);
}
fn main() {
    print(fields("a,b,c", ","));
    print(fields("no separator", ","));
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n1\n");
}
#[test]
fn allocation_failure_reports_the_operation_location() {
    let source = "fn main() {\n    let text = \"hello\";\n    print(text);\n}";
    let exhausted = execute_with_env(source, &[("AE_ALLOC_LIMIT", "0")]);
    assert!(!exhausted.status.success(), "an exhausted allocation budget must fail");
    let stderr = String::from_utf8_lossy(&exhausted.stderr);
    assert!(stderr.contains("out of memory"), "{stderr}");
    assert!(stderr.contains("ae:2:"), "the location of the string literal is missing: {stderr}");
    let generous = execute_with_env(source, &[("AE_ALLOC_LIMIT", "4096")]);
    assert!(generous.status.success(), "{}", String::from_utf8_lossy(&generous.stderr));
    assert_eq!(String::from_utf8_lossy(&generous.stdout).trim(), "hello");
    // A malformed limit is ignored rather than silently disabling the program.
    let malformed = execute_with_env(source, &[("AE_ALLOC_LIMIT", "lots")]);
    assert!(malformed.status.success(), "{}", String::from_utf8_lossy(&malformed.stderr));
}
#[test]
fn private_items_stay_inside_their_file() {
    let dir = std::env::temp_dir().join(format!("ae-priv-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("lib.ae"), "struct Hidden { n: i64 }\nenum Mood { Calm, Loud }\nfn secret() -> i64 { return 7; }\npub fn open() -> i64 { return 8; }\npub struct Shown { v: i64 }\n").unwrap();
    let main_file = dir.join("main.ae");
    let check = |body: &str| {
        fs::write(&main_file, format!("use \"lib.ae\";\nfn main() {{ {body} }}")).unwrap();
        Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&main_file).output().unwrap()
    };
    let ok = check("let s = Shown { v: 1 }; print(open() + s.v);");
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    for (body, expected) in [
        ("print(secret());", "function 'secret' is private to"),
        ("let h = Hidden { n: 1 }; print(h.n);", "struct 'Hidden' is private to"),
        ("let m = Mood::Calm; print(open());", "enum 'Mood' is private to"),
    ] {
        let output = check(body);
        assert!(!output.status.success(), "accepted: {body}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{body}: {err}");
    }
    // A file sees its own private items, so the library itself compiles.
    fs::write(dir.join("alone.ae"), "struct Hidden { n: i64 }\nfn secret() -> i64 { return 7; }\nfn main() { let h = Hidden { n: secret() }; print(h.n); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(dir.join("alone.ae")).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "7");
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn trim_and_prefix_checks_work_on_bytes() {
    let output = execute(r#"fn main() {
    print("[" + trim("  padded \n") + "]");
    print(trim("   ") == "");
    print(trim("中文  "));
    print(starts_with("notes.txt", "note"));
    print(starts_with("notes.txt", "x"));
    print(ends_with("notes.txt", ".txt"));
    print(ends_with("a.txt.bak", ".txt"));
    print(starts_with("x", "") && ends_with("x", ""));
    print(ends_with("abc", "abcd"));
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "[padded]\ntrue\n中文\ntrue\nfalse\ntrue\nfalse\ntrue\nfalse\n"
    );
}
#[test]
fn trim_and_prefix_checks_reject_wrong_argument_types() {
    for source in [
        "fn main() { print(trim(1)); }",
        "fn main() { print(starts_with(1, \"a\")); }",
        "fn main() { print(ends_with(\"a\", 2)); }",
        "fn main() { print(starts_with(\"a\")); }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-trim-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
#[test]
fn generic_enums_instantiate_per_argument() {
    let output = execute(r#"enum Either[T, U] { Left(T), Right(U), Both(T, U), Neither }
fn locate(text: &string, needle: &string) -> Option[i64] {
    let parts = split(text, needle);
    if len(parts) < 2 { return Option[i64]::None; }
    return Option[i64]::Some(len(parts[0]));
}
fn label(opt: Option[i64]) -> string {
    match opt {
        Option::Some(v) => { return "at " + to_string(v); }
        Option::None => { return "absent"; }
    }
}
fn main() {
    let hit = locate("a,b,c", ",");
    match hit {
        Option::Some(v) => { print("hit " + to_string(v)); }
        Option::None => { print("miss"); }
    }
    print(label(locate("abc", ",")));
    print(label(locate("xy;z", ";")));
    let e = Either[string, i64]::Both("n", 3);
    match e {
        Either::Left(text) => { print("left " + text); }
        Either::Right(n) => { print(n); }
        Either::Both(text, n) => { print(text + "=" + to_string(n)); }
        Either::Neither => { print("neither"); }
    }
    let many = Either[i64, vec[string]]::Right(split("p q", " "));
    match many {
        Either::Left(n) => { print(n); }
        Either::Right(parts) => { print(len(parts)); }
        Either::Both(n, parts) => { print(n); }
        Either::Neither => { print("neither"); }
    }
    let absent = Option[vec[string]]::None;
    match absent {
        Option::Some(parts) => { print(len(parts)); }
        Option::None => { print("no vector"); }
    }
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "hit 1\nabsent\nat 2\nn=3\n2\nno vector\n"
    );
}
#[test]
fn generic_enums_reuse_one_layout_per_argument_list() {
    // Two mentions of Option[string] must be the same type: the second match
    // would be rejected as a type mismatch otherwise.
    let output = execute(r#"fn pick(flag: bool) -> Option[string] {
    if flag { return Option[string]::Some("yes"); }
    return Option[string]::None;
}
fn main() {
    let a = pick(true);
    let b = pick(false);
    match a {
        Option::Some(v) => { print(v); }
        Option::None => { print("none"); }
    }
    match b {
        Option::Some(v) => { print(v); }
        Option::None => { print("none"); }
    }
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "yes\nnone\n");
}
#[test]
fn invalid_generic_enums_are_rejected() {
    for (source, expected) in [
        ("fn main() { let a = Option; }", "'Option' is a generic enum"),
        ("fn main() { let a = Option[i64, i64]::Some(1); }", "takes 1 type argument(s), found 2"),
        ("struct P { x: i64 }\nfn main() { let a = Option[P]::None; }", "type arguments must be i64"),
        ("struct B[T, T] { v: i64 }\nfn main() { print(1); }", "duplicate type parameter"),
        ("enum P[t] { A(t) }\nfn main() { print(1); }", "type parameters are single uppercase letters"),
        ("enum Option { Here, Gone }\nfn main() { print(1); }", "duplicate enum 'Option'"),
        ("enum P[T] { A(T), B(T) }\nfn main() { let p = P[i64]::A(1); match p { P::A(v) => { print(v); } } }", "match is not exhaustive"),
        ("fn main() { let a = Option[i64]::Some(\"x\"); }", "payload 1 of 'Option[i64]::Some' must be i64"),
        ("fn main() { let a = Option[string]::Some(\"x\"); let b = a; print(len(a)); }", "because it was moved"),
        ("fn t(o: Option) -> i64 { return 1; }\nfn main() { print(t()); }", "is a generic enum"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-generic-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}
#[test]
fn generic_functions_can_instantiate_a_struct_with_the_same_internal_id() {
    let source = "struct Boxed[T] { value: T }
                  fn boxed[T](value: T) -> Boxed[T] { return Boxed[T] { value: value }; }
                  fn main() { let item = boxed[i64](7); print(item.value); }";
    let path = std::env::temp_dir().join(format!(
        "ae-generic-layout-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
#[test]
fn identical_concrete_arguments_share_one_instance() {
    // Two call sites with the same argument list reuse one layout; a
    // different list is a second instance. The C emitter gives every layout
    // a release helper named ae_t<N>_release, which is countable.
    let emit = |program: &str| -> String {
        let path = std::env::temp_dir().join(format!(
            "ae-instance-{}-{}.ae",
            std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, program).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("emit-c").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(output.status.success(), "{}",
            String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let layouts = |c: &str| -> usize {
        c.lines()
            .filter(|line| line.starts_with("static void ae_t"))
            .filter(|line| line.contains("_release("))
            .count()
    };
    let same = emit(concat!(
        "struct Box[T] { v: T }\n",
        "fn main() {\n",
        "    let a = Box[i64] { v: 1 };\n",
        "    let b = Box[i64] { v: 2 };\n",
        "    print(a.v + b.v);\n",
        "}\n"
    ));
    assert_eq!(layouts(&same), 1, "{same}");
    let different = emit(concat!(
        "struct Box[T] { v: T }\n",
        "fn main() {\n",
        "    let a = Box[i64] { v: 1 };\n",
        "    let b = Box[string] { v: \"s\" };\n",
        "    print(a.v);\n",
        "    print(b.v);\n",
        "}\n"
    ));
    assert_eq!(layouts(&different), 2, "{different}");
}

#[test]
fn nested_generic_layouts_report_the_full_budget_trace() {
    let mut source = String::from("struct Layer0[T] { value: T }\n");
    for index in 1..=256 {
        source.push_str(&format!(
            "struct Layer{index}[T] {{ value: Layer{}[T] }}\n", index - 1
        ));
    }
    source.push_str("fn main() { let layer = Layer256[i64] { value: 0 }; }");
    let path = std::env::temp_dir().join(format!(
        "ae-generic-chain-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    let expected = (0..=256).rev().map(|index| format!("Layer{index}[i64]"))
        .collect::<Vec<_>>().join(" -> ");
    assert!(diagnostic.contains(&format!(
        "generic instantiation budget exceeded (256 struct types) at 'Layer0' (instantiated: {expected})"
    )), "{diagnostic}");
}
#[test]
fn generic_instantiation_budget_is_enforced() {
    // Four parameters and four argument types give 256 combinations, which is
    // past the budget: the point is a diagnostic, not a compiler OOM.
    let mut source = String::from("enum Tag[T, U, V, W] { Here, There }\nfn main() {\n");
    let types = ["i64", "bool", "string", "vec"];
    let mut index = 0;
    for a in types { for b in types { for c in types { for d in types {
        source.push_str(&format!("    let t{index} = Tag[{a}, {b}, {c}, {d}]::Here;\n"));
        index += 1;
    } } } }
    source.push_str("    print(1);\n}\n");
    let path = std::env::temp_dir().join(format!("ae-generic-budget-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, &source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(!output.status.success(), "a program past the instantiation budget must be rejected");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("generic instantiation budget exceeded"), "{err}");
    assert!(err.contains("instantiated: Tag["), "{err}");
}
#[test]
fn generic_enums_respect_file_visibility() {
    let dir = std::env::temp_dir().join(format!("ae-generic-vis-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("lib.ae"), "pub enum Found[T] { Here(T), Gone }\nfn hidden(flag: bool) -> Found[i64] { if flag { return Found[i64]::Here(1); } return Found[i64]::Gone; }\npub fn open(flag: bool) -> Found[i64] { return hidden(flag); }\n").unwrap();
    let main_file = dir.join("main.ae");
    let check = |body: &str| {
        fs::write(&main_file, format!("use \"lib.ae\";\nfn main() {{ {body} }}")).unwrap();
        Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&main_file).output().unwrap()
    };
    // The exported enum is usable, and so is the exported function returning it.
    let ok = check("let f = open(true); match f { Found::Here(v) => { print(v); } Found::Gone => { print(0); } }");
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    // A private function is still private, whatever type it returns.
    let bad = check("let f = hidden(true); print(1);");
    assert!(!bad.status.success());
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(err.contains("'hidden' is private to"), "{err}");
    // A private generic enum is not instantiable from another file either.
    fs::write(dir.join("secret.ae"), "enum Quiet[T] { Here(T), Gone }\npub fn one() -> i64 { return 1; }\n").unwrap();
    fs::write(&main_file, "use \"secret.ae\";\nfn main() { let q = Quiet[i64]::Gone; print(1); }").unwrap();
    let bad = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&main_file).output().unwrap();
    assert!(!bad.status.success());
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(err.contains("enum 'Quiet[i64]' is private to"), "{err}");
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn sort_and_reverse_order_vectors_in_place() {
    let output = execute(r#"fn main() {
    var numbers = vec[3, 1, 2];
    sort(numbers);
    print(numbers[0]);
    print(numbers[1]);
    print(numbers[2]);
    reverse(numbers);
    print(numbers[0]);
    var names = split("pear apple fig", " ");
    sort(names);
    print(names[0]);
    print(names[2]);
    reverse(names);
    print(names[0]);
    var empty = vec[];
    sort(empty);
    reverse(empty);
    print(len(empty));
    var one = vec[7];
    sort(one);
    print(one[0]);
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "1\n2\n3\n3\napple\npear\npear\n0\n7\n"
    );
}
#[test]
fn sort_requires_a_mutable_vector_and_keeps_borrows_out() {
    for source in [
        "fn main() { sort(1); }",
        "fn main() { let v = vec[1]; sort(v); }",
        "fn main() { var s = \"text\"; sort(s); }",
        "fn main() { var v = vec[1]; for x in v { sort(v); } }",
    ] {
        let path = std::env::temp_dir().join(format!("ae-sort-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
#[test]
fn clock_reports_a_plausible_epoch() {
    let output = execute("fn main() { let seconds = now_s(); print(seconds > 1600000000); print(now_ms() >= seconds * 1000); print(now_ms() < (seconds + 2) * 1000); }");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\ntrue\ntrue\n");
}
#[test]
fn environment_lookup_separates_missing_from_empty() {
    let source = r#"fn show(name: &string) -> string {
    let found = env(name);
    match found {
        ReadResult::Ok(value) => { return "[" + value + "]"; }
        ReadResult::Err(reason) => { return "missing"; }
    }
}
fn main() {
    print(show("AE_TEST_PRESENT"));
    print(show("AE_TEST_ABSENT"));
}"#;
    let output = execute_with_env(source, &[("AE_TEST_PRESENT", "value")]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "[value]\nmissing\n");
    // A variable that exists but holds nothing is still a value, not an error.
    let output = execute_with_env(source, &[("AE_TEST_PRESENT", "")]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "[]\nmissing\n");
    // An empty name is a caller mistake, reported rather than looked up.
    let output = execute_with_env("fn main() { let r = env(\"\"); match r { ReadResult::Ok(v) => { print(\"found\"); } ReadResult::Err(e) => { print(e); } } }", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "variable name is empty");
}
#[test]
fn append_adds_to_a_file_that_write_truncates() {
    let path = std::env::temp_dir().join(format!("ae-append-{}-{}.txt", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_file(&path);
    let literal = path.display().to_string().replace('\\', "/");
    let source = format!(r#"fn main() {{
    let first = try_append_file("{literal}", "one\n");
    match first {{ WriteResult::Ok(n) => {{ print(n); }} WriteResult::Err(r) => {{ print(r); }} }}
    let second = try_append_file("{literal}", "two\n");
    match second {{ WriteResult::Ok(n) => {{ print(n); }} WriteResult::Err(r) => {{ print(r); }} }}
    let text = read_file("{literal}");
    print(len(text));
    let replaced = try_write_file("{literal}", "gone");
    match replaced {{ WriteResult::Ok(n) => {{ print(n); }} WriteResult::Err(r) => {{ print(r); }} }}
    print(read_file("{literal}"));
    let missing = try_append_file("{literal}/nested/x.txt", "x");
    match missing {{ WriteResult::Ok(n) => {{ print("unexpected"); }} WriteResult::Err(r) => {{ print("failed"); }} }}
}}"#, literal = literal);
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "4\n4\n8\n4\ngone\nfailed\n"
    );
    let _ = fs::remove_file(&path);
}
#[test]
fn exit_sets_the_status_and_flushes_what_was_printed() {
    let output = execute(r#"fn main() {
    let note = "about to stop";
    print(note);
    exit(3);
}"#);
    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "about to stop\n");
    // Code after a terminating statement is never reached.
    let output = execute("fn main() { print(\"before\"); exit(0); print(\"after\"); }");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "before\n");
}
#[test]
fn exit_can_end_a_function_that_never_returns_a_value() {
    let output = execute(r#"fn stop(code: i64) -> i64 {
    let note = "stopping";
    print(note);
    exit(code);
}
fn main() {
    let reached = stop(4);
    print(reached);
}"#);
    assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "stopping\n");
}
#[test]
fn exit_rejects_non_integer_status_codes() {
    for source in ["fn main() { exit(\"no\"); }", "fn main() { exit(true); }", "fn main() { exit(); }"] {
        let path = std::env::temp_dir().join(format!("ae-exit-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
    }
}
fn execute_with_stdin(source: &str, input: &[u8]) -> std::process::Output {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("ae-stdin-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    fs::remove_file(path).unwrap();
    output
}
#[test]
fn standard_input_arrives_as_bytes_without_newline_translation() {
    let source = r#"fn main() {
    let input = read_stdin();
    match input {
        ReadResult::Ok(text) => { print(len(text)); print(contains(text, "\r")); }
        ReadResult::Err(reason) => { print("cannot read: " + reason); }
    }
}"#;
    let output = execute_with_stdin(source, b"a\r\nb");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\ntrue\n");
    // Nothing on the other end is an empty read, not a failure.
    let output = execute_with_stdin(source, b"");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\nfalse\n");
}
#[test]
fn working_directory_is_a_usable_path() {
    let output = execute(r#"fn main() {
    let here = cwd();
    match here {
        ReadResult::Ok(path) => { print(path_is_dir(path)); print(len(path) > 1); }
        ReadResult::Err(reason) => { print("cannot tell: " + reason); }
    }
}"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "true\ntrue\n");
}
#[test]
fn the_pipe_stats_example_counts_a_piped_stream() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/pipe_stats.ae")).unwrap();
    let output = execute_with_stdin(&source, "中文行\nshort\n".as_bytes());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "bytes 16\ncharacters 10\nlines 2\nlongest line 5\n"
    );
}
#[test]
fn json_paths_select_members_elements_and_escapes() {
    let source = r#"fn main() {
    let doc = "{ \"name\": \"ae\", \"tags\": [\"lang\", \"fast\"], \"meta\": { \"stars\": 12, \"flag\": true }, \"note\": null, \"text\": \"caf\\u00e9\\n\" }";
    let a = json_get(doc, "name");
    match a { ReadResult::Ok(v) => { print(v); } ReadResult::Err(e) => { print(e); } }
    let b = json_get(doc, "tags[1]");
    match b { ReadResult::Ok(v) => { print(v); } ReadResult::Err(e) => { print(e); } }
    let c = json_int(doc, "meta.stars");
    match c { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(e) => { print(e); } }
    let d = json_bool(doc, "meta.flag");
    match d { BoolResult::Ok(f) => { print(f); } BoolResult::Err(e) => { print(e); } }
    let e = json_get(doc, "note");
    match e { ReadResult::Ok(v) => { print(v); } ReadResult::Err(x) => { print(x); } }
    let f = json_get(doc, "text");
    match f { ReadResult::Ok(v) => { print(v); print(len(v)); } ReadResult::Err(x) => { print(x); } }
    let g = json_len(doc, "tags");
    match g { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(x) => { print(x); } }
    print(json_has(doc, "meta.stars"));
    print(json_has(doc, "missing"));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "ae\nfast\n12\ntrue\nnull\ncafé\n\n6\n2\ntrue\nfalse\n"
    );
}

#[test]
fn json_errors_separate_missing_from_wrong_type() {
    let source = r#"fn main() {
    let doc = "{ \"s\": \"x\", \"n\": 3, \"arr\": [1], \"big\": 99999999999999999999, \"f\": 1.5 }";
    let a = json_get(doc, "missing");
    match a { ReadResult::Ok(v) => { print(v); } ReadResult::Err(e) => { print(e); } }
    let b = json_int(doc, "s");
    match b { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(e) => { print(e); } }
    let c = json_bool(doc, "n");
    match c { BoolResult::Ok(f) => { print(f); } BoolResult::Err(e) => { print(e); } }
    let d = json_len(doc, "s");
    match d { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(e) => { print(e); } }
    let e = json_get(doc, "arr[3]");
    match e { ReadResult::Ok(v) => { print(v); } ReadResult::Err(x) => { print(x); } }
    let f = json_get(doc, "s.x");
    match f { ReadResult::Ok(v) => { print(v); } ReadResult::Err(x) => { print(x); } }
    let g = json_int(doc, "big");
    match g { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(x) => { print(x); } }
    let h = json_int(doc, "f");
    match h { IntResult::Ok(n) => { print(to_string(n)); } IntResult::Err(x) => { print(x); } }
    let i = json_get(doc, "a.");
    match i { ReadResult::Ok(v) => { print(v); } ReadResult::Err(x) => { print(x); } }
    let j = json_get("{ \"a\": }", "a");
    match j { ReadResult::Ok(v) => { print(v); } ReadResult::Err(x) => { print(x); } }
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        concat!(
            "no member named 'missing'\n",
            "'s' is a string, not a number\n",
            "'n' is a number, not a boolean\n",
            "'s' is a string; only arrays and objects have a member count\n",
            "path segment [3] is past the end of the array\n",
            "path segment 'x' needs an object, found a string\n",
            "'big' does not fit in i64\n",
            "'f' is not an integer\n",
            "path ends with '.'\n",
            "JSON syntax error near byte 0\n",
        )
    );
}
#[test]
fn cache_prunes_entries_beyond_the_configured_limit() {
    let dir = std::env::temp_dir().join(format!("ae-cache-limit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    let source = dir.join("prog.ae");
    fs::create_dir(&dir).unwrap();
    fs::write(&source, "fn main() { print(\"limited\"); }").unwrap();
    let stats = || {
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("cache").arg("stats")
            .env("AE_CACHE", &cache).env("AE_CACHE_LIMIT", "1").output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let build = |out: &std::path::Path, limit: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ae"));
        cmd.arg("build").arg(&source).arg("-o").arg(out).env("AE_CACHE", &cache).env("AE_TIMINGS", "1");
        match limit {
            Some(value) => { cmd.env("AE_CACHE_LIMIT", value); }
            None => { cmd.env_remove("AE_CACHE_LIMIT"); }
        }
        cmd.output().unwrap()
    };
    // One byte is below any entry, so publishing is followed by an immediate
    // prune: the build succeeds and the cache ends up empty.
    let first = dir.join(if cfg!(windows) { "one.exe" } else { "one" });
    let out = build(&first, Some("1"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // `frontend entries:` also ends in `entries: 0`, so compare the first line
    // instead of searching the whole report.
    assert_eq!(stats().lines().next().unwrap_or(""), "entries: 0", "{}", stats());
    assert!(stats().contains("limit: 1"), "{}", stats());
    let output = Command::new(&first).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "limited");
    // Without the cap the entry survives, so the cache is usable at all.
    let second = dir.join(if cfg!(windows) { "two.exe" } else { "two" });
    let out = build(&second, None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let after = String::from_utf8_lossy(&Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("cache").arg("stats").env("AE_CACHE", &cache).env_remove("AE_CACHE_LIMIT").output().unwrap().stdout).into_owned();
    assert_eq!(after.lines().next().unwrap_or(""), "entries: 1", "{after}");
    assert!(after.contains("limit: none"), "{after}");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn string_vectors_start_empty_and_copy_borrowed_elements() {
    // `args()` hands back owned strings, but indexing it gives a borrow. Pushing
    // a borrow must copy it: moving the element would empty the slot it was read
    // from, and the vector would free the same buffer twice on the way out.
    let source = r#"fn main() {
    let argv = args();
    var copy = strvec_new();
    for i in 0..len(argv) { push(copy, argv[i]); }
    print(len(copy));
    print(len(argv));
    print(join(copy, "|") == join(argv, "|"));
}"#;
    let path = std::env::temp_dir().join(format!("ae-strvec-copy-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&path).arg("--").arg("one").arg("two").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3\n3\ntrue\n");
}

#[test]
fn run_command_reports_status_and_output() {
    // One child invocation that must exist on both platforms, plus one program
    // name that cannot exist: a failed start is an Err, a non-zero exit is not.
    let (program, arguments) = if cfg!(windows) {
        ("cmd", vec!["/c", "echo", "child-ok"])
    } else {
        ("sh", vec!["-c", "echo child-ok"])
    };
    let pushes = arguments.iter().map(|a| format!("    push(argv, \"{a}\");\n")).collect::<String>();
    let source = format!(r#"fn main() {{
    var argv = strvec_new();
{pushes}    let result = run_command("{program}", argv);
    match result {{
        CommandResult::Ok(finished) => {{ print(finished.status); print(contains(finished.output, "child-ok")); }}
        CommandResult::Err(reason) => {{ print("failed: " + reason); }}
    }}
    var empty = strvec_new();
    let missing = run_command("ae-no-such-program-on-path", empty);
    match missing {{
        CommandResult::Ok(finished) => {{ print("unexpected " + to_string(finished.status)); }}
        CommandResult::Err(reason) => {{ print(contains(reason, "cannot start")); }}
    }}
}}"#);
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\ntrue\ntrue\n");
}

#[test]
fn emit_mir_decomposes_statements_into_bindings() {
    // The MIR (M2.2): statements decompose into place = rvalue bindings with
    // explicit temporaries, over the CFG's block graph.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-emit-mir-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn add(a: i64, b: i64) -> i64 { return a + b; }
fn main() { print(add(2, 3)); }
").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("fn main() -> i64"), "main prints: {mir}");
    assert!(mir.contains("= fn#"), "call decomposes into a binding: {mir}");
    assert!(mir.contains("fn #0(v0: i64, v1: i64) -> i64"), "signatures print: {mir}");
}

#[test]
fn emit_mir_preserves_vector_index_types() {
    let path = std::env::temp_dir().join(format!(
        "ae-mir-vector-index-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, r#"fn read(values: &vec[f64], words: &vec[string]) -> i64 {
    print(values[0]);
    print(words[0]);
    return 0;
}
fn main() {
    let values = vec[1.5];
    let words = split("a b", " ");
    let _result = read(&values, &words);
}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("(f64) = index(v0 (&vec[f64]), 0) -> f64"), "{mir}");
    assert!(mir.contains("(string) = index(v1 (&vec[string]), 0) -> string"), "{mir}");
}

#[test]
fn emit_mir_runs_clean_on_the_run_suite() {
    // Every construct the run suite uses must lower into MIR without a
    // validator panic or a missing arm.
    let suite = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run");
    let mut checked = 0;
    for entry in fs::read_dir(&suite).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("ae") { continue; }
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(entry.path()).output().unwrap();
        assert!(output.status.success(), "emit-mir failed on {}: {}", entry.path().display(), String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "emit-mir printed nothing for {}", entry.path().display());
        checked += 1;
    }
    assert!(checked > 50, "the run suite shrank: {checked} files");
}

#[test]
fn emit_cfg_shapes_loops_branches_and_switches() {
    // The CFG layer (M2.2 slice 2): control flow lowers into blocks with
    // terminators, break/continue become explicit edges, and `match` becomes
    // a switch. The statement contents ride along verbatim.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-emit-cfg-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "enum E { A(i64), B }
fn pick(e: E) -> i64 { match e { E::A(x) => { return x; } E::B => { return 0; } } }
fn main() { let r = pick(E::A(7)); print(r); }
").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-cfg").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cfg = String::from_utf8_lossy(&output.stdout);
    assert!(cfg.contains("switch v0"), "match lowers to a switch: {cfg}");
    assert!(cfg.contains("return"), "arms terminate: {cfg}");
}

#[test]
fn move_walk_agrees_with_the_parser_on_the_run_suite() {
    // M2.5 L1: the CFG move walk runs as part of every compile; these
    // programs exercise moves, re-initialization, borrows, and loop
    // back-edges, and the walk must accept everything the parser accepts.
    let suite = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run");
    let mut checked = 0;
    for entry in fs::read_dir(&suite).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("ae") { continue; }
        let source = fs::read_to_string(entry.path()).unwrap();
        // Programs that intentionally fail at runtime still compile clean.
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-tree").arg(entry.path()).output().unwrap();
        assert!(output.status.success(), "compile failed for {}: {}", entry.path().display(), String::from_utf8_lossy(&output.stderr));
        let _ = source;
        checked += 1;
    }
    assert!(checked > 50, "the run suite shrank: {checked} files");
}

#[test]
fn emit_cfg_runs_clean_on_the_run_suite() {
    // Every construct the run suite uses must lower into a validated graph.
    let suite = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run");
    let mut checked = 0;
    for entry in fs::read_dir(&suite).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("ae") { continue; }
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-cfg").arg(entry.path()).output().unwrap();
        assert!(output.status.success(), "emit-cfg failed on {}: {}", entry.path().display(), String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "emit-cfg printed nothing for {}", entry.path().display());
        checked += 1;
    }
    assert!(checked > 50, "the run suite shrank: {checked} files");
}

#[test]
fn emit_tree_preserves_negation_and_multiplication_grouping() {
    use ae_syntax::{expression::{parse_operator_expression, Expression, ExpressionKind}, Kind};

    fn shape(expression: &Expression) -> String {
        match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => value.to_string(),
            ExpressionKind::Group(inner) => shape(inner),
            ExpressionKind::Prefix { operator: Kind::Minus, operand } => format!("(-{})", shape(operand)),
            ExpressionKind::Binary { left, operator: Kind::Star, right } => {
                format!("({} * {})", shape(left), shape(right))
            }
            other => panic!("unexpected expression node: {other:?}"),
        }
    }

    for (source, expected) in [
        ("-2 * 3", "((-2) * 3)"),
        ("-(2 * 3)", "(-(2 * 3))"),
        ("2 * -3", "(2 * (-3))"),
    ] {
        let parsed = parse_operator_expression("main.ae", source).unwrap();
        assert_eq!(parsed.range, 0..source.len());
        assert_eq!(shape(&parsed), expected);
    }

    let dir = unique_temp_dir("operator-grouping");
    let path = dir.join("main.ae");
    fs::write(&path, "fn main() { print(-2 * 3); print(-(2 * 3)); print(2 * -3); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-tree").arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let tree = String::from_utf8_lossy(&output.stdout);
    let expressions: Vec<_> = tree.lines().filter_map(|line| line.strip_prefix("  print ")).collect();
    assert_eq!(expressions, [
        "-2 @ 1:19 * 3 (i64 @ 1:22)",
        "-2 * 3 (i64 @ 1:38) @ 1:34",
        "2 * -3 @ 1:55 (i64 @ 1:53)",
    ]);
}

#[test]
fn standalone_literals_agree_with_the_compiler_on_values_and_spans() {
    use ae_syntax::{expression::{parse_operator_expression, ExpressionKind}, Kind};

    // The token keeps the source spelling (escapes uninterpreted) and the
    // runtime decodes exactly the four escapes below. Expected bytes are
    // spelled out here, independently of the compiler's own decoder.
    let cases = [
        (r#""plain""#, "plain"),
        (r#""a\nb""#, "a\nb"),
        (r#""tab\there""#, "tab\there"),
        (r#""carriage\rreturn""#, "carriage\rreturn"),
        (r#""quote\"end""#, "quote\"end"),
        (r#""back\\slash""#, "back\\slash"),
        (r#""héllo世界""#, "héllo世界"),
        (r#""""#, ""),
    ];
    // Recomputed from bytes: the compiler reports 1-based character columns.
    let position = |source: &str, offset: usize| {
        let prefix = &source[..offset];
        let line = prefix.matches('\n').count() + 1;
        let start = prefix.rfind('\n').map(|newline| newline + 1).unwrap_or(0);
        (line, source[start..offset].chars().count() + 1)
    };
    let ae = env!("CARGO_BIN_EXE_ae");

    for (spelling, decoded) in cases {
        let parsed = parse_operator_expression("main.ae", spelling).unwrap();
        let ExpressionKind::Atom(Kind::Text(raw)) = &parsed.kind else {
            panic!("expected a text atom for {spelling}");
        };
        assert_eq!(parsed.range, 0..spelling.len(), "{spelling}");
        assert_eq!(raw, &spelling[1..spelling.len() - 1], "{spelling}");

        let source = format!("fn main() {{\n    let s = {spelling};\n    print(s);\n}}\n");
        let dir = unique_temp_dir("literal");
        let path = dir.join("main.ae");
        fs::write(&path, &source).unwrap();
        let tree = Command::new(ae).arg("emit-tree").arg(&path).output().unwrap();
        let run = Command::new(ae).arg("run").arg(&path).output().unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(tree.status.success(), "{spelling}: {}", String::from_utf8_lossy(&tree.stderr));
        assert!(run.status.success(), "{spelling}: {}", String::from_utf8_lossy(&run.stderr));

        let offset = source.find(spelling).unwrap();
        let (line, column) = position(&source, offset);
        assert_eq!(&source[offset..offset + spelling.len()], spelling);
        let needle = format!("{:?} @ {line}:{column}", raw);
        let dumped = String::from_utf8_lossy(&tree.stdout);
        assert!(dumped.contains(&needle), "{spelling}: expected {needle:?} in\n{dumped}");

        let printed = String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n");
        assert_eq!(printed, format!("{decoded}\n"), "{spelling}");
    }

    // A multibyte prefix on the same line separates the two units: the token
    // range is a byte offset, the reported column counts characters.
    let source = "fn main() {\n    let a = \"héllo世界\"; let s = \"a\\nb\";\n    print(s);\n}\n";
    let offset = source.rfind("\"a\\nb\"").unwrap();
    let (line, column) = position(source, offset);
    let byte_column = source[..offset].rsplit('\n').next().unwrap().len() + 1;
    assert_ne!(column, byte_column, "the fixture must separate the two units");
    let dir = unique_temp_dir("literal-multibyte");
    let path = dir.join("main.ae");
    fs::write(&path, source).unwrap();
    let tree = Command::new(ae).arg("emit-tree").arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    let needle = format!("{:?} @ {line}:{column}", "a\\nb");
    let dumped = String::from_utf8_lossy(&tree.stdout);
    assert!(dumped.contains(&needle), "expected {needle:?} in\n{dumped}");
}

#[test]
fn standalone_and_compiler_agree_on_string_literal_rejections() {
    // Each fixture holds one broken literal; the compiler's JSON diagnostic
    // must name the same code and byte range the extracted lexer reports.
    let cases = [
        ("fn main() { print(\"oops); }", "E0003"),
        ("fn main() {\n    print(\"bad\\q\");\n}", "E0002"),
        ("fn main() {\n    print(\"line\nbreak\");\n}", "E0004"),
    ];
    let ae = env!("CARGO_BIN_EXE_ae");
    for (source, code) in cases {
        let lexer = ae_syntax::lex_diagnostic(source).unwrap_err();
        assert_eq!(lexer.code.as_deref(), Some(code), "{source:?}");
        let range = lexer.primary.as_ref().expect("primary label").span.range.clone();

        let dir = unique_temp_dir("literal-rejection");
        let path = dir.join("main.ae");
        fs::write(&path, source).unwrap();
        let output = Command::new(ae).args(["check", "--json"]).arg(&path).output().unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(output.status.code(), Some(1), "{source:?}");
        let json = String::from_utf8_lossy(&output.stdout);
        assert!(json.contains(&format!("\"code\":\"{code}\"")), "{source:?}: {json}");
        assert!(
            json.contains(&format!("\"byte_start\":{},\"byte_end\":{}", range.start, range.end)),
            "{source:?}: expected {range:?} in {json}"
        );
    }
}

#[test]
fn standalone_postfix_expression_matches_compiler_execution() {
    let expression = "-combine(values[0], point.x) * 3";
    let parsed = ae_syntax::expression::parse_operator_expression("main.ae", expression).unwrap();
    assert_eq!(parsed.range, 0..expression.len());

    let source = format!(
        r#"struct Point {{ x: i64 }}
fn combine(a: i64, b: i64) -> i64 {{ return a + b; }}
fn main() {{
    let values = vec[4];
    let point = Point {{ x: 5 }};
    print({expression});
}}"#
    );
    let output = execute(&source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "-27");
}

#[test]
fn json_check_does_not_merge_different_declaration_errors_at_the_same_span() {
    let source = "fn first() -> { }\nfn second(value: i64 { }\nfn main() {}";
    let syntax = ae_syntax::declaration::recover_declarations("main.ae", source);
    assert_eq!(syntax.diagnostics.len(), 2);
    let range = &syntax.diagnostics[0].primary.as_ref().unwrap().span.range;
    let dir = unique_temp_dir("declaration-recovery-different-error");
    let path = dir.join("main.ae");
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", "--json"]).arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let json = String::from_utf8_lossy(&output.stdout);
    assert_eq!(json.matches("\"message\":").count(), 1, "{json}");
    assert!(json.contains("\"message\":\"expected identifier\""), "{json}");
    assert!(json.contains(&format!("\"byte_start\":{},\"byte_end\":{}", range.start, range.end)), "{json}");
}

#[test]
fn json_check_recovers_independent_declaration_errors() {
    let source = "struct First ;\nstruct Second { value i64 }\nfn second(value: i64 { }\nfn main() {}";
    let dir = unique_temp_dir("declaration-recovery");
    let path = dir.join("main.ae");
    fs::write(&path, source).unwrap();
    let compiler = env!("CARGO_BIN_EXE_ae");
    let strict = Command::new(compiler).arg("check").arg(&path).output().unwrap();
    let recovered = Command::new(compiler).args(["check", "--json"]).arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(strict.status.code(), Some(1));
    assert_eq!(recovered.status.code(), Some(1));
    let json = String::from_utf8_lossy(&recovered.stdout);
    assert_eq!(json.matches("\"message\":").count(), 3, "{json}");
    assert_eq!(json.matches("\"code\":\"E0010\"").count(), 3, "{json}");
    for (marker, len, message) in [
        (";", 1, "expected LBrace, found Semi"),
        ("i64 }\nfn second", 3, "expected Colon, found Ident(\"i64\")"),
        ("{ }\nfn main", 1, "expected RParen, found LBrace"),
    ] {
        let offset = source.find(marker).unwrap();
        let escaped = message.replace('"', "\\\"");
        let expected = format!("\"message\":\"{escaped}\",\"byte_start\":{offset},\"byte_end\":{}", offset + len);
        assert!(json.contains(&expected), "{json}");
    }
}

#[test]
fn standalone_declarations_cover_example_and_run_corpus() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for directory in ["examples", "examples/modules", "tests/run"] {
        for entry in fs::read_dir(root.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("ae") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            let file = path.display().to_string();
            #[allow(clippy::result_large_err)]
            let parsed = (|| {
                let tokens = ae_syntax::lex_diagnostic(&source)?;
                let headers = ae_syntax::module::parse_use_headers(&file, &source, &tokens)?;
                ae_syntax::declaration::parse_declaration_tokens(
                    &file, &source, &tokens, headers.body_start,
                )
            })();
            match parsed {
                Ok(declarations) => assert_eq!(
                    declarations,
                    ae_syntax::declaration::parse_declarations(&file, &source).unwrap(),
                    "{file}"
                ),
                Err(error) => failures.push(format!("{file}: {error}")),
            }
        }
    }
    failures.sort();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn syntax_body_boundaries_preserve_following_compiler_declarations() {
    use ae_syntax::{declaration::{brace_body_end, parse_declarations, DeclarationKind}, lex_diagnostic, Kind};

    let source = r#"fn earlier(n: i64) -> i64 { if true { print("}"); } return n; }
struct Boxed[T] { value: T }
fn main() { print(earlier(7)); }"#;
    let tokens = lex_diagnostic(source).unwrap();
    let function_body = tokens.iter().position(|token| token.kind == Kind::LBrace).unwrap();
    let struct_start = brace_body_end(&tokens, function_body + 1).unwrap();
    assert_eq!(tokens[struct_start].kind, Kind::Struct);
    assert_eq!(tokens[struct_start].offset, source.find("struct Boxed").unwrap());
    let struct_body = tokens.iter().enumerate().skip(struct_start)
        .find(|(_, token)| token.kind == Kind::LBrace).unwrap().0;
    let main_start = brace_body_end(&tokens, struct_body + 1).unwrap();
    assert_eq!(tokens[main_start].kind, Kind::Fn);
    assert_eq!(tokens[main_start].offset, source.find("fn main").unwrap());

    let declarations = parse_declarations("main.ae", source).unwrap();
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[0].range.end, source.find("\nstruct Boxed").unwrap());
    assert_eq!(declarations[1].range.start, tokens[struct_start].offset);
    assert_eq!(declarations[2].range.start, tokens[main_start].offset);
    let DeclarationKind::Function { parameters, returns: Some(returns), .. } = &declarations[0].kind else {
        panic!("expected typed function");
    };
    assert_eq!(parameters[0].name, "n");
    assert_eq!(&source[parameters[0].ty.clone()], "i64");
    assert_eq!(&source[returns.clone()], "i64");
    let DeclarationKind::Struct { fields } = &declarations[1].kind else {
        panic!("expected generic struct");
    };
    assert_eq!(declarations[1].type_parameters[0].name, "T");
    assert_eq!(fields[0].name, "value");
    assert_eq!(&source[fields[0].ty.clone()], "T");

    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "}\n7\n");
}

#[test]
fn emit_tree_dumps_the_lowered_statement_tree() {
    // `emit-tree` is the inspectable layer above the parser (M2.2 slice 1):
    // it prints declarations and every statement with its expression, and it
    // must run clean over the whole suite's constructs.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-emit-tree-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "struct P { x: i64 }
fn main() { let p = P { x: 1 }; print(p.x); }
").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-tree").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success());
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(tree.contains("struct #"), "struct declarations print: {tree}");
    assert!(tree.contains("fn main() -> i64 {"), "main prints: {tree}");
    assert!(tree.contains("v0: owned = struct #2 {"), "literal prints: {tree}");
    assert!(tree.contains("print v0 (struct #2) @ 2:39.x"), "field read prints: {tree}");
}

#[test]
fn emit_tree_runs_clean_on_the_run_suite() {
    // Every construct the run suite uses must lower and print without a
    // panic or a missing printer arm.
    let suite = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run");
    let mut checked = 0;
    for entry in fs::read_dir(&suite).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("ae") { continue; }
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-tree").arg(entry.path()).output().unwrap();
        assert!(output.status.success(), "emit-tree failed on {}: {}", entry.path().display(), String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "emit-tree printed nothing for {}", entry.path().display());
        checked += 1;
    }
    assert!(checked > 50, "the run suite shrank: {checked} files");
}

#[test]
fn emit_mir_shows_which_releases_survive_drop_elaboration() {
    // M2.4: the printed graph is the one the C backend ships — releases
    // included. A release list prints as a `releases(...)` binding with one
    // operand per slot, and the drop pass rewrites both views together: the
    // moved slot's release vanishes from the print, the unproven one stays.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-emit-mir-drops-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn take(x: string) -> i64 { return 1; }\nfn moved() -> i64 {\n    let s = \"hello\";\n    print(take(s));\n    return 0;\n}\nfn branchy(c: bool) -> i64 {\n    let s = \"hello\";\n    if c { print(take(s)); }\n    return 0;\n}\nfn main() { print(moved()); print(branchy(true)); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    // `take` releases its parameter: the release prints with the slot's type.
    let take_body = mir.split("fn #0").nth(1).and_then(|rest| rest.split("fn #1").next()).unwrap_or("");
    assert!(take_body.contains("releases(v0 (string))"), "the parameter release prints: {mir}");
    // `moved` consumed its binding with the call, so its release is gone
    // from both views.
    let moved_body = mir.split("fn #1").nth(1).and_then(|rest| rest.split("fn #2").next()).unwrap_or("");
    assert!(!moved_body.contains("releases("), "the moved slot's release is elided from the print: {mir}");
    // `branchy`'s binding is live on one path and moved on the other: no
    // proof decides, so the release stays visible.
    let branchy_body = mir.split("fn #2").nth(1).and_then(|rest| rest.split("fn main").next()).unwrap_or("");
    assert!(branchy_body.contains("releases(v1 (string))"), "the unproven release prints: {mir}");
}

#[test]
fn field_granularity_borrows_freeze_their_own_field_only() {
    // M2.5 L2: a view of `p.name` freezes writes to `p.name` while the view
    // can still be read, but a write to a different field releases bytes the
    // view does not point at — allowed, and the run proves both fields
    // behave. The viewed field's write while the view is read later is the
    // rejection.
    let dir = unique_temp_dir("field_granularity");
    let source = dir.join("fields.ae");
    fs::write(&source, "struct P { name: string, note: string }
fn main() {
    var p = P { name: \"hello\", note: \"n\" };
    let a = slice_view(p.name, 0, 1);
    let b = slice_view(p.note, 0, 1);
    print(a);
    print(b);
    p.note = \"changed\";
    print(p.name);
    print(p.note);
}
").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "run failed: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "h\nn\nhello\nchanged\n");

    let viewed = dir.join("viewed.ae");
    fs::write(&viewed, "struct P { name: string, note: string }
fn main() {
    var p = P { name: \"hello\", note: \"n\" };
    let a = slice_view(p.name, 0, 1);
    p.name = \"changed\";
    print(a);
}
").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&viewed).output().unwrap();
    assert!(!rejected.status.success(), "writing the viewed field while it is read later must be rejected");
    let err = String::from_utf8_lossy(&rejected.stderr);
    assert!(err.contains("may still be read"), "the NLL diagnostic names the conflict: {err}");

    let moved = dir.join("moved.ae");
    fs::write(&moved, "struct P { name: string, note: string }
fn main() {
    var p = P { name: \"hello\", note: \"n\" };
    let a = slice_view(p.name, 0, 1);
    let q = p;
    print(a);
}
").unwrap();
    let rejected_move = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&moved).output().unwrap();
    assert!(!rejected_move.status.success(), "moving the whole struct while a field view is read later must be rejected");
    let err = String::from_utf8_lossy(&rejected_move.stderr);
    assert!(err.contains("may still be read"), "the NLL diagnostic names the conflict: {err}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn constant_array_indices_in_range_skip_the_bounds_check() {
    // M2.10: a constant index into a fixed array carries its length in the
    // type, so an in-range constant is provably in bounds and the runtime
    // check elides from the C. An out-of-range constant keeps the check —
    // the trap is runtime behavior, like division by zero.
    let dir = unique_temp_dir("bounds_elision");
    let source = dir.join("arrays.ae");
    fs::write(&source, "fn main() {
    var arr = [10, 20, 30];
    arr[1] = 99;
    print(arr[0]);
    print(arr[1]);
    print(arr[2]);
}
").unwrap();
    let emit = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&source).output().unwrap();
    assert!(emit.status.success(), "{}", String::from_utf8_lossy(&emit.stderr));
    let c = String::from_utf8_lossy(&emit.stdout);
    assert!(!c.contains("ae_index"), "in-range constant indices elide the check: {c}");
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "10\n99\n30\n");

    let oob = dir.join("oob.ae");
    fs::write(&oob, "fn main() {
    var arr = [10, 20, 30];
    print(arr[7]);
}
").unwrap();
    let trap = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&oob).output().unwrap();
    assert!(!trap.status.success(), "an out-of-range constant still traps");
    assert!(String::from_utf8_lossy(&trap.stderr).contains("array index out of bounds"), "the trap names the failure: {}", String::from_utf8_lossy(&trap.stderr));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mir_interpreter_agrees_on_file_builtins_with_the_compiled_backends() {
    // M2.11: the file builtins interpret against the real filesystem —
    // writes, reads, metadata, byte-sorted listings, path operations,
    // and removals — and must agree with the compiled backend line for
    // line. The error payload TEXT differs (the C runtime strerror
    // wording vs the interp io::Error wording — programs cannot rely
    // on it), so the probe exercises success paths, whose payloads are
    // exact. The probe files live in the process CWD and are cleaned
    // up afterwards.
    let source = "probe_files.ae";
    fs::write(source, "fn main() {\n    let _c = try_create_dir(\"./probe_dir\");\n    let w = try_write_file(\"./probe_dir/probe.txt\", \"hello file\");\n    match w { WriteResult::Ok(n) => { print(n); } WriteResult::Err(_) => { print(0 - 1); } }\n    let text = read_file(\"./probe_dir/probe.txt\");\n    print(text);\n    let st = try_file_info(\"./probe_dir/probe.txt\");\n    match st {\n        FileResult::Ok(meta) => { print(meta.size); }\n        FileResult::Err(_) => { print(0 - 1); }\n    }\n    let _c2 = try_create_dir(\"./probe_dir/sub\");\n    let _w2 = try_write_file(\"./probe_dir/sub/inner.txt\", \"inner\");\n    let listed = try_list_dir(\"./probe_dir/sub\");\n    match listed {\n        DirResult::Ok(names) => { print(len(names)); print(names[0]); }\n        DirResult::Err(e) => { print(e); }\n    }\n    print(path_exists(\"./probe_dir/sub\"));\n    print(path_is_dir(\"./probe_dir/sub\"));\n    print(path_join(\"dir\", \"name\"));\n    print(path_name(\"dir/sub/name.txt\"));\n    print(path_parent(\"dir/sub/name.txt\"));\n    let _r1 = try_remove_file(\"./probe_dir/probe.txt\");\n    let _r2 = try_remove_file(\"./probe_dir/sub/inner.txt\");\n    let _r3 = try_remove_dir(\"./probe_dir/sub\");\n    print(path_exists(\"./probe_dir/probe.txt\"));\n}").unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", source, "--interp"]).output().unwrap();
    assert!(interp.status.success(), "interp failed: {}", String::from_utf8_lossy(&interp.stderr));
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(source).output().unwrap();
    assert!(compiled.status.success(), "compiled failed: {}", String::from_utf8_lossy(&compiled.stderr));
    let normalize = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let expected = "10\nhello file\n10\n1\ninner.txt\ntrue\ntrue\ndir/name\nname.txt\ndir/sub\nfalse\n";
    assert_eq!(normalize(&interp.stdout), normalize(&compiled.stdout), "the interpreter and the compiled backend disagree");
    assert_eq!(normalize(&interp.stdout), expected);
    let _ = fs::remove_file("probe_files.ae");
    let _ = fs::remove_dir_all("probe_dir");
}

#[test]
fn mir_interpreter_agrees_on_token_cursors_with_the_compiled_backends() {
    // M2.11: the tokenizer's cursor — creation, per-iteration token views
    // with empty segments retained, the reset that makes a second loop over
    // the same cursor behave like the first, and now_ms's real clock —
    // must agree with the compiled backends line for line.
    let dir = unique_temp_dir("interp_tokens");
    let source = dir.join("tokens.ae");
    fs::write(&source, "fn main() {
    var text = \"alpha,beta,,gamma\";
    let cur = tokenize(text, \",\");
    for t in cur {
        print(t);
        print(len(t));
    }
    print(now_ms() > 0);
    for t in cur {
        print(t);
    }
    var pairs = 0;
    for outer in cur {
        for inner in cur { pairs = pairs + 1; }
    }
    print(pairs);
}
").unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", source.to_str().unwrap(), "--interp"]).output().unwrap();
    assert!(interp.status.success(), "interp failed: {}", String::from_utf8_lossy(&interp.stderr));
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(compiled.status.success(), "compiled failed: {}", String::from_utf8_lossy(&compiled.stderr));
    let normalize = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let expected = "alpha\n5\nbeta\n4\n\n0\ngamma\n5\ntrue\nalpha\nbeta\n\ngamma\n16\n";
    assert_eq!(normalize(&interp.stdout), normalize(&compiled.stdout), "the interpreter and the compiled backend disagree");
    assert_eq!(normalize(&interp.stdout), expected);
    let tree = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run").arg(&source).env("AE_CODEGEN", "tree").output().unwrap();
    assert!(tree.status.success(), "tree failed: {}", String::from_utf8_lossy(&tree.stderr));
    assert_eq!(normalize(&tree.stdout), expected);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mir_interpreter_agrees_on_result_enums_with_the_compiled_backends() {
    // M2.11: map_get and parse_int construct the prelude's result enums —
    // the hit/miss/parse-error payloads must match the runtime's constants
    // ("key not found", "not a base-10 integer") byte for byte, in both the
    // interpreter and the compiled backends.
    let dir = unique_temp_dir("interp_results");
    let source = dir.join("results.ae");
    fs::write(&source, "fn main() {
    var hit = map_new();
    hit[\"k\"] = 42;
    let found = map_get(hit, \"k\");
    match found {
        IntResult::Ok(n) => { print(n); }
        IntResult::Err(e) => { print(e); }
    }
    let miss = map_get(hit, \"nope\");
    match miss {
        IntResult::Ok(n) => { print(n); }
        IntResult::Err(e) => { print(e); }
    }
    let parsed = parse_int(\"42\");
    match parsed {
        IntResult::Ok(n) => { print(n); }
        IntResult::Err(e) => { print(e); }
    }
    let bad = parse_int(\"4x2\");
    match bad {
        IntResult::Ok(n) => { print(n); }
        IntResult::Err(e) => { print(e); }
    }
}
").unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", source.to_str().unwrap(), "--interp"]).output().unwrap();
    assert!(interp.status.success(), "interp failed: {}", String::from_utf8_lossy(&interp.stderr));
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(compiled.status.success(), "compiled failed: {}", String::from_utf8_lossy(&compiled.stderr));
    let normalize = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let expected = "42\nkey not found\n42\nnot a base-10 integer\n";
    assert_eq!(normalize(&interp.stdout), normalize(&compiled.stdout), "the interpreter and the compiled backend disagree");
    assert_eq!(normalize(&interp.stdout), expected);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mir_interpreter_agrees_on_maps_with_the_compiled_backends() {
    // M2.11: the interpreter's map family — puts, incs (insert-or-
    // increment), predicates, sorted keys, removals — must agree with the
    // compiled backends line for line.
    let dir = unique_temp_dir("interp_maps");
    let source = dir.join("maps.ae");
    fs::write(&source, "fn main() {
    var counts = map_new();
    counts[\"a\"] = 5;
    let bumped = map_inc(counts, \"a\", 2);
    print(bumped);
    print(len(counts));
    if map_has(counts, \"b\") { print(1); } else { print(0); }
    let set = map_inc(counts, \"b\", 6);
    print(set);
    let ks = keys(counts);
    print(ks[0]);
    print(ks[1]);
    print(len(ks));
    let removed = map_remove(counts, \"b\");
    print(removed);
    print(len(counts));
}
").unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", source.to_str().unwrap(), "--interp"]).output().unwrap();
    assert!(interp.status.success(), "interp failed: {}", String::from_utf8_lossy(&interp.stderr));
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(compiled.status.success(), "compiled failed: {}", String::from_utf8_lossy(&compiled.stderr));
    let normalize = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    assert_eq!(normalize(&interp.stdout), normalize(&compiled.stdout), "the interpreter and the compiled backend disagree");
    assert_eq!(normalize(&interp.stdout), "7\n1\n0\n6\na\nb\n2\ntrue\n1\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn string_vector_assignment_reads_old_element_before_replacement() {
    let dir = unique_temp_dir("strvec_self_assignment");
    let source = dir.join("assign.ae");
    fs::write(&source, "fn main() { var values = strvec_new(); push(values, \"hello\"); push(values, \"world\"); values[0] = values[0]; print(values[0]); values[1] = values[0]; print(values[1]); values[0] = \"new\"; print(values[0]); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "hello\nhello\nnew\n");
    }
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).arg("--interp").output().unwrap();
    assert!(interp.status.success(), "interp: {}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"), "hello\nhello\nnew\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn field_assignment_evaluates_rhs_before_releasing_old_value() {
    let dir = unique_temp_dir("field_self_reference");
    let source = dir.join("field.ae");
    fs::write(&source, "struct Record { value: string }\nfn main() { var r = Record { value: \"hello\" }; r.value = r.value + \"!\"; print(r.value); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "hello!\n");
    }
    let vector = dir.join("vector.ae");
    fs::write(&vector, "struct Record { value: vec[f64] }\nfn main() { var r = Record { value: vec[1.0] }; let replacement = vec[2.0]; r.value = replacement; print(\"done\"); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let emitted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&vector).env("AE_CODEGEN", backend).output().unwrap();
        assert!(emitted.status.success(), "{backend}: {}", String::from_utf8_lossy(&emitted.stderr));
        let c = String::from_utf8_lossy(&emitted.stdout);
        assert!(c.contains("ae_gvec_release(&v0.value);"), "{backend}: old vector field must be released");
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&vector).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "done\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closure_can_query_captured_map() {
    let dir = unique_temp_dir("closure_read_map");
    let source = dir.join("capture.ae");
    fs::write(&source, "fn main() { var values = map_new(); values[\"key\"] = 42; let exists = || -> bool { return map_has(values, \"key\"); }; print(exists()); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "true\n");
    }
    for (index, (text, diagnostic)) in [
        ("fn main() { var values = map_new(); values[\"key\"] = 42; let exists = || -> bool { return map_has(values, \"key\"); }; print(exists()); print(map_has(values, \"key\")); }\n", "moved"),
        ("fn main() { var values = map_new(); let remove = || -> bool { return map_remove(values, \"key\"); }; print(remove()); }\n", "cannot mutate captured"),
    ].iter().enumerate() {
        let invalid = dir.join(format!("invalid{index}.ae"));
        fs::write(&invalid, text).unwrap();
        let checked = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&invalid).output().unwrap();
        assert!(!checked.status.success(), "invalid map capture accepted: {text}");
        assert!(String::from_utf8_lossy(&checked.stderr).contains(diagnostic), "{}", String::from_utf8_lossy(&checked.stderr));
    }
    fs::remove_dir_all(dir).unwrap();
}



#[test]
fn concatenation_keeps_a_capture_borrowed_and_returning_it_moves() {
    // String plus copies (spec/generics.md): a body that only concatenates
    // passes the borrow probe, so the closure stays reusable and the outer
    // binding keeps its value. Returning the capture needs ownership, which
    // makes every call a move.
    let concatenating = [
        "fn main() {",
        "    let note = \"memo\";",
        "    let shout = || -> string { return note + \"!\"; };",
        "    print(shout());",
        "    print(shout());",
        "    print(note);",
        "}",
    ].join("\n");
    let output = execute(&concatenating);
    assert!(output.status.success(), "{}",
        String::from_utf8_lossy(&output.stderr));
    let printed: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines().map(str::to_owned).collect();
    assert_eq!(printed, ["memo!", "memo!", "memo"]);

    let returning = [
        "fn main() {",
        "    let note = \"memo\";",
        "    let hand = || -> string { return note; };",
        "    print(hand());",
        "    print(hand());",
        "}",
    ].join("\n");
    let path = std::env::temp_dir().join(format!(
        "ae-capture-move-{}-{}.ae",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, returning).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(concat!(
        "cannot call 'hand' again because a value capture was already ",
        "moved by the first call"
    )), "{error}");
}

#[test]
fn closure_captures_token_separator() {
    let dir = unique_temp_dir("closure_token_separator");
    let source = dir.join("capture.ae");
    fs::write(&source, "fn main() { let text = \"a b\"; let separator = \" \"; let count = || -> i64 { let cursor = tokenize(text, separator); var total = 0; for part in cursor { total = total + 1; } return total; }; print(count()); let more = \"c d\"; let cursor = tokenize(more, \" \"); let count_captured = || -> i64 { var total = 0; for part in cursor { total = total + 1; } return total; }; print(count_captured()); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "2\n2\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closure_captures_vector_and_index_expression() {
    let dir = unique_temp_dir("closure_index_capture");
    let source = dir.join("capture.ae");
    fs::write(&source, "fn main() { let values = vec[10, 42]; let index = 1; let get = || -> i64 { return values[index]; }; print(get()); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closure_captures_function_value_and_its_argument() {
    let dir = unique_temp_dir("closure_function_capture");
    let source = dir.join("capture.ae");
    fs::write(&source, "fn apply(f: fn(i64) -> i64, x: i64) -> i64 { let call = || -> i64 { return f(x); }; return call(); }\nfn main() { print(apply(|n: i64| -> i64 { return n + 1; }, 41)); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closure_captures_pointer_offset_count() {
    let dir = unique_temp_dir("closure_pointer_count");
    let source = dir.join("capture.ae");
    fs::write(&source, "fn main() { var value = 42; let pointer = &value as *const i64; let amount = 0; let get = || -> i64 { unsafe { let shifted = pointer.offset(amount); return shifted.deref(); } }; print(get()); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pointer_offset_count_can_consume_owned_argument() {
    let dir = unique_temp_dir("pointer_offset_owned_count");
    let source = dir.join("offset.ae");
    fs::write(&source, "fn offset(s: string) -> i64 { print(s); return 0; }\nfn main() { var value = 42; let pointer = &value as *const i64; let text = \"owned\"; unsafe { let shifted = pointer.offset(offset(text)); print(shifted.deref()); } }\n").unwrap();
    for backend in ["tree", "mir"] {
        let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "owned\n42\n");
    }
    let invalid = dir.join("moved.ae");
    fs::write(&invalid, "fn offset(s: string) -> i64 { return 0; }\nfn main() { var value = 42; let pointer = &value as *const i64; let text = \"owned\"; unsafe { let shifted = pointer.offset(offset(text)); print(shifted.deref()); } print(text); }\n").unwrap();
    let checked = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&invalid).output().unwrap();
    assert!(!checked.status.success(), "use after move was accepted");
    assert!(String::from_utf8_lossy(&checked.stderr).contains("moved"), "{}", String::from_utf8_lossy(&checked.stderr));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nested_repr_c_struct_crosses_a_separate_c_translation_unit() {
    let dir = unique_temp_dir("nested_c_ffi");
    let source = dir.join("main.ae");
    fs::write(&source, "#[repr(C)] struct Inner { flag: bool, value: i64 }\n#[repr(C)] struct Outer { prefix: bool, inner: Inner, tail: bool }\nextern \"C\" fn nested_ffi_sum(value: Outer) -> i64;\nfn main() { let inner = Inner { flag: true, value: 40 }; let outer = Outer { prefix: true, inner: inner, tail: true }; unsafe { print(nested_ffi_sum(outer)); } }\n").unwrap();
    let foreign = dir.join("foreign.c");
    fs::write(&foreign, "#include <stdint.h>\n#include <stdbool.h>\nstruct Inner { bool flag; int64_t value; };\nstruct Outer { bool prefix; struct Inner inner; bool tail; };\nint64_t nested_ffi_sum(struct Outer value) { return value.inner.value + (value.prefix ? 1 : 0) + (value.inner.flag && value.tail ? 1 : 0); }\n").unwrap();
    for backend in ["tree", "mir"] {
        let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&source).env("AE_CODEGEN", backend).output().unwrap();
        assert!(generated.status.success(), "{backend}: {}", String::from_utf8_lossy(&generated.stderr));
        let generated_path = dir.join(format!("{backend}.c"));
        fs::write(&generated_path, generated.stdout).unwrap();
        let executable = dir.join(if cfg!(windows) { format!("{backend}.exe") } else { backend.to_owned() });
        let compiler = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
        let mut cc = Command::new(compiler);
        cc.arg("-std=c11").arg(&generated_path).arg(&foreign).arg("-o").arg(&executable);
        if !cfg!(windows) { cc.arg("-lm"); }
        let output = cc.output().unwrap();
        assert!(output.status.success(), "{backend}: {}", String::from_utf8_lossy(&output.stderr));
        let run = Command::new(executable).output().unwrap();
        assert!(run.status.success(), "{backend}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repr_c_nested_layout_matches_the_native_c_compiler() {
    let dir = unique_temp_dir("nested_c_layout");
    let source = dir.join("nested.ae");
    fs::write(&source, "#[repr(C)] struct Inner { flag: bool, value: i64 }\n#[repr(C)] struct Outer { prefix: bool, inner: Inner, tail: bool }\nfn main() { let inner = Inner { flag: true, value: 42 }; let outer = Outer { prefix: false, inner: inner, tail: true }; print(c_size_of[Outer]()); print(c_align_of[Outer]()); print(c_offset_of[Outer](\"inner\")); print(c_offset_of[Outer](\"tail\")); }\n").unwrap();
    let c = dir.join("nested.c");
    let executable = dir.join(if cfg!(windows) { "nested.exe" } else { "nested" });
    fs::write(&c, "#include <stdbool.h>\n#include <stdint.h>\n#include <stddef.h>\n#include <stdio.h>\nstruct Inner { bool flag; int64_t value; };\nstruct Outer { bool prefix; struct Inner inner; bool tail; };\nint main(void) { printf(\"%zu\\n%zu\\n%zu\\n%zu\\n\", sizeof(struct Outer), _Alignof(struct Outer), offsetof(struct Outer, inner), offsetof(struct Outer, tail)); return 0; }\n").unwrap();
    let cc = Command::new("gcc").arg("-std=c11").arg(&c).arg("-o").arg(&executable).output().unwrap();
    assert!(cc.status.success(), "{}", String::from_utf8_lossy(&cc.stderr));
    let native = Command::new(&executable).output().unwrap();
    assert!(native.status.success());
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), String::from_utf8_lossy(&native.stdout).replace("\r\n", "\n"));
    let bad = dir.join("bad.ae");
    fs::write(&bad, "struct Plain { x: i64 } #[repr(C)] struct Outer { inner: Plain } fn main() {}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&bad).output().unwrap();
    assert!(!output.status.success(), "non-repr(C) nested field must be rejected");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn c_alignment_matches_the_native_c_compiler() {
    let dir = unique_temp_dir("scalar_alignment");
    let source = dir.join("layout.ae");
    fs::write(&source, "#[repr(C)] struct Mixed { a: bool, b: i64 }\nfn alignment[T](v: T) -> i64 { return c_align_of[T](); }\nfn main() { print(c_size_of[i64]()); print(c_align_of[i64]()); print(c_size_of[f64]()); print(c_align_of[f64]()); print(c_size_of[bool]()); print(c_align_of[bool]()); print(c_size_of[Mixed]()); print(c_align_of[Mixed]()); print(c_offset_of[Mixed](\"b\")); print(alignment[bool](true)); let m = Mixed { a: true, b: 1 }; print(alignment[Mixed](m)); }\n").unwrap();
    let c = dir.join("layout.c");
    let executable = dir.join(if cfg!(windows) { "layout.exe" } else { "layout" });
    fs::write(&c, "#include <stdint.h>\n#include <stdbool.h>\n#include <stddef.h>\n#include <stdio.h>\nstruct Mixed { bool a; int64_t b; };\nint main(void) { printf(\"%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n%zu\\n\", sizeof(int64_t), _Alignof(int64_t), sizeof(double), _Alignof(double), sizeof(bool), _Alignof(bool), sizeof(struct Mixed), _Alignof(struct Mixed), offsetof(struct Mixed, b), _Alignof(bool), _Alignof(struct Mixed)); return 0; }\n").unwrap();
    let compile = Command::new("gcc").arg("-std=c11").arg(&c).arg("-o").arg(&executable).output().unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    let native = Command::new(&executable).output().unwrap();
    assert!(native.status.success());
    for mode in [None, Some("--interp"), Some("--release")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&source);
        if let Some(mode) = mode { command.arg(mode); }
        let run = command.output().unwrap();
        assert!(run.status.success(), "{mode:?}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), String::from_utf8_lossy(&native.stdout).replace("\r\n", "\n"));
    }
    for (index, text) in ["fn main() { print(c_align_of[string]()); }", "struct Plain { a: i64 } fn main() { print(c_align_of[Plain]()); }", "fn main() { print(c_align_of[Missing]()); }"].iter().enumerate() {
        let path = dir.join(format!("bad{index}.ae"));
        fs::write(&path, text).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        assert!(!output.status.success(), "accepted: {text}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn scalar_size_queries_in_generic_and_concrete_contexts() {
    let dir = unique_temp_dir("scalar_size");
    let source = dir.join("scalar_size.ae");
    fs::write(&source, "fn size[T](v: T) -> i64 { return c_size_of[T](); }\nfn main() { print(c_size_of[i64]()); print(c_size_of[f64]()); print(c_size_of[bool]()); print(size[i64](1)); print(size[f64](1.0)); print(size[bool](true)); }\n").unwrap();
    for mode in [None, Some("--interp"), Some("--release")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&source);
        if let Some(mode) = mode { command.arg(mode); }
        let run = command.output().unwrap();
        assert!(run.status.success(), "{mode:?}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "8\n8\n1\n8\n8\n1\n");
    }
    let cases = [
        ("fn main() { print(c_offset_of[i64](\"x\")); }", "not a struct"),
        ("fn main() { print(c_size_of[string]()); }", "not a struct"),
        ("fn main() { print(c_size_of[Missing]()); }", "not a struct"),
        ("fn size[T](v: T) -> i64 { return c_size_of[T](); } fn main() { print(size[string](\"x\")); }", "not a struct"),
    ];
    for (index, (text, expected)) in cases.iter().enumerate() {
        let path = dir.join(format!("bad{index}.ae"));
        fs::write(&path, text).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        assert!(!output.status.success(), "accepted: {text}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{text}: {}", String::from_utf8_lossy(&output.stderr));
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repr_c_layout_queries_and_extern_prototypes() {
    // M2.12 U8: `#[repr(C)]` pins a struct's layout — declaration
    // order, natural alignment, trailing padding — and the compile-time
    // query pair pins it in a test. The `extern "C"` prototype emits
    // with the struct as a by-value parameter, so a foreign definition
    // (coming with U7's #[no_mangle]) links against a real type.
    let dir = unique_temp_dir("repr_c");
    let source = dir.join("pair.ae");
    fs::write(&source, "#[repr(C)] struct Pair { a: i64, b: i64 }\nextern \"C\" fn pair_sum(p: Pair) -> i64;\nimpl Pair { fn sum(&self) -> i64 { return self.a + self.b; } }\nfn main() {\n    let p = Pair { a: 10, b: 32 };\n    print(c_size_of[Pair]());\n    print(c_offset_of[Pair](\"a\"));\n    print(c_offset_of[Pair](\"b\"));\n    print(p.sum());\n}").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "run failed: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "16\n0\n8\n42\n");

    // The prototype names the struct type; the layout queries folded to
    // constants in the C.
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&source).output().unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let c = String::from_utf8_lossy(&emitted.stdout).replace("\r\n", "\n");
    assert!(c.contains("int64_t pair_sum(ae_t2 v0);"), "the by-value prototype: {c}");
    assert!(!c.contains("ae_index"), "no runtime layout checks: {c}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn extern_prototypes_follow_declaration_order() {
    let dir = unique_temp_dir("extern_prototype_order");
    let source = dir.join("main.ae");
    fs::write(&source, "extern \"C\" fn second_by_name() -> i64;\nextern \"C\" fn first_by_name() -> i64;\nfn main() {}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-c")
        .arg(&source)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let c = String::from_utf8(output.stdout).unwrap();
    let second = c.find("int64_t second_by_name(void);").unwrap();
    let first = c.find("int64_t first_by_name(void);").unwrap();
    assert!(second < first, "foreign declarations changed order");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repr_c_attribute_guards_and_extern_struct_rejections() {
    // The attribute's three guards and the E0703 crossing rejection.
    let cases = [
        ("#[repr(C)] struct Empty {} fn main() {}",
         "must have at least one field"),
        ("struct Boxed[T] { value: T } #[repr(C)] struct Outer { item: Boxed[i64] } fn main() {}",
         "cannot claim a C layout"),
        ("fn size_of[T](v: &T) -> i64 { return len(v); } #[repr(C)] struct Boxed[V] { inner: V } fn main() {}",
         "is generic, so it has no single C layout"),
        ("#[repr(C)] struct Bad { name: string } fn main() {}",
         "owns storage, so the struct cannot claim a C layout"),
        ("struct Plain { a: i64 } extern \"C\" fn take(p: Plain) -> i64; fn main() {}",
         "cannot cross the C ABI"),
        ("struct Plain { a: i64 } fn main() { print(c_size_of[Plain]()); }",
         "its layout is the compiler's choice"),
        ("#[repr(C)] struct P { a: i64 } fn main() { print(c_offset_of[P](\"missing\")); }",
         "has no field"),
    ];
    let dir = unique_temp_dir("repr_c_guards");
    for (index, (source, expected)) in cases.iter().enumerate() {
        let path = dir.join(format!("case{index}.ae"));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        assert!(!output.status.success(), "accepted: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unsafe_fn_doc_comment_is_enforced_and_a_documented_one_runs() {
    // M2.12 U10 (spec/unsafe.md 6): the compiler enforces the presence
    // of an `unsafe fn`'s precondition doc comment — it cannot enforce
    // truth. A documented unsafe fn runs like any other.
    let dir = unique_temp_dir("unsafe_doc");
    let source = dir.join("scale.ae");
    fs::write(&source, "/// Preconditions: the caller guarantees `x` is a valid, aligned i64\n/// value; no aliasing concerns (by-value scalar); single-threaded context.\nunsafe fn scale(x: i64) -> i64 { unsafe { return x * 2; } }\nfn main() {\n    unsafe { print(scale(21)); }\n}").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "run failed: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");

    let missing = dir.join("missing.ae");
    fs::write(&missing, "unsafe fn raw(x: i64) -> i64 { unsafe { return x; } }\nfn main() {}\n").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&missing).output().unwrap();
    assert!(!rejected.status.success(), "an undocumented unsafe fn must be rejected");
    let err = String::from_utf8_lossy(&rejected.stderr);
    assert!(err.contains("doc comment naming its preconditions"), "the E0704 diagnostic: {err}");

    // A prose comment block without any `///` line is not a doc comment.
    let prose = dir.join("prose.ae");
    fs::write(&prose, "// raw(x): x must be a valid i64.\nunsafe fn raw(x: i64) -> i64 { unsafe { return x; } }\nfn main() {}\n").unwrap();
    let rejected_prose = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&prose).output().unwrap();
    assert!(!rejected_prose.status.success(), "a prose comment is not a doc comment");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_mangle_exposure_runs_bidirectionally_and_stays_c_visible() {
    // M2.12 U7: `#[no_mangle]` on an `extern "C" fn` with a body keeps
    // the symbol C-visible with external linkage. The runtime's
    // trampoline calls it exactly as a real C caller would — through a
    // foreign declaration of a symbol defined later in the same unit —
    // and main calls it directly as ae code.
    let dir = unique_temp_dir("no_mangle");
    let source = dir.join("exposure.ae");
    fs::write(&source, "#[no_mangle]\nextern \"C\" fn test_add(a: i64, b: i64) -> i64 { return a + b; }\nextern \"C\" fn ae_exposure_trampoline(a: i64, b: i64) -> i64;\nfn main() {\n    unsafe { print(ae_exposure_trampoline(20, 22)); }\n    print(test_add(1, 2));\n}").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "run failed: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n3\n");

    // The emitted C: the exposure under its own name, external linkage,
    // and the prototypes both declarations need.
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&source).output().unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let c = String::from_utf8_lossy(&emitted.stdout).replace("\r\n", "\n");
    assert!(c.contains("int64_t test_add(int64_t v0, int64_t v1) {"), "the exposure definition is non-static: {c}");
    assert!(c.contains("int64_t ae_exposure_trampoline(int64_t v0, int64_t v1);"), "the declaration prototype: {c}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_mangle_attribute_misuse_is_rejected() {
    // The attribute belongs on an `extern "C" fn` definition: on any
    // other item (or on a bodyless declaration, whose symbol name is
    // already the C name) it is rejected by name.
    let cases = [
        ("#[no_mangle]\nfn plain() { }\nfn main() {}", "applies to an"),
        ("#[no_mangle]\nextern \"C\" fn probe(x: i64) -> i64;\nfn main() {}", "applies to an"),
        ("#[repr(C)]\nextern \"C\" fn probe(x: i64) -> i64;\nfn main() {}", "applies to a struct"),
        ("#[missing]\nfn main() {}", "unknown attribute"),
    ];
    let dir = unique_temp_dir("no_mangle_guards");
    for (index, (source, expected)) in cases.iter().enumerate() {
        let path = dir.join(format!("case{index}.ae"));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        assert!(!output.status.success(), "accepted: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unnecessary_unsafe_block_warns_without_failing_the_compile() {
    // M2.12 U4 (spec/unsafe.md W0702): an `unsafe` block whose body
    // contains no unchecked operation is silent decoration — the compile
    // carries a warning on stderr and still succeeds; a block with a raw
    // dereference (nested here, to prove the recursive walk) stays
    // silent.
    let dir = unique_temp_dir("w0702");
    let source = dir.join("w0702.ae");
    fs::write(&source, "fn main() {\n    unsafe { print(1 + 2); }\n    var x = 41;\n    unsafe { let p = &x as *const i64; unsafe { print(p.deref()); } }\n}").unwrap();
    // `run` builds and executes; the warning rides stderr either way.
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "the compile still succeeds: {}", String::from_utf8_lossy(&run.stderr));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("warning: unnecessary unsafe block (W0702)"), "the decoration warns: {stderr}");
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "3\n41\n");
    fs::remove_dir_all(dir).unwrap();
}

#[allow(dead_code)] // kept as the documented M2.4 propagate-elision probe; superseded by the MIR differential suite
fn mir_drop_elision_thins_propagate_error_path_releases() {
    // M2.4: a `?`'s releases ride the error path, which exits the function,
    // so the state to read them with is the one after the value evaluated.
    // A slot the program already moved out of before the `?` reads as
    // dropped there, and its error-path release is elided; the binding the
    // program still holds keeps its release, as does the scope end.
    let dir = unique_temp_dir("drop_propagate");
    let source = dir.join("propagate.ae");
    fs::write(&source, "fn try_val(fail: bool) -> IntResult {\n    if fail { return IntResult::Err(\"x\"); }\n    return IntResult::Ok(1);\n}\nfn probe(fail: bool) -> IntResult {\n    let s = \"hello\";\n    let t = s + \"!\";\n    let n = try_val(fail)?;\n    return IntResult::Ok(n);\n}\nfn main() {\n    let a = probe(false);\n    let b = probe(true);\n    match a { IntResult::Ok(x) => { print(x); } IntResult::Err(_) => { print(0 - 1); } }\n    match b { IntResult::Ok(x) => { print(x); } IntResult::Err(_) => { print(0 - 1); } }\n}\n").unwrap();
    let emit = |mode: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .args(["emit-c", source.to_str().unwrap()])
            .env("AE_CODEGEN", mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
    };
    let mir = emit("mir");
    let tree = emit("tree");
    let releases_in_probe = |text: &str| -> usize {
        // The definition, not the prototype: a prototype ends in `);`.
        let body = text.split("ae_fn_1(bool v0) {").nth(1).unwrap_or("");
        let end = body.find("\nstatic ").map(|n| body[..n].len()).unwrap_or(body.len());
        body[..end].matches("ae_str_release(&v").count()
    };
    // Two survive: `t` on the error path and `t` at the scope end. Elided:
    // `s` twice — the concat consumed it before the `?` and before the end.
    assert_eq!(releases_in_probe(&mir), 2, "the moved slot's releases are elided from both paths: {mir}");
    assert!(releases_in_probe(&tree) > 2, "the tree backend keeps the conservative releases: {tree}");
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir_out = String::from_utf8_lossy(&run("mir").stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&run("tree").stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out, "backends disagree");
    assert_eq!(mir_out, "1\n-1\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emit_mir_lowers_closures_and_aggregates() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-emit-mir-closures-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "struct Pair { a: i64, b: i64 }\nfn add(p: Pair) -> i64 { return p.a + p.b; }\nfn main() { let f = |x: i64| -> i64 { return x + 1; }; let p = Pair { a: f(2), b: 4 }; print(add(p)); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("closure#0"), "closure literal lowers to ClosureLit: {mir}");
    assert!(mir.contains("struct #2 {"), "struct aggregate lowers: {mir}");
    assert!(mir.contains("-> i64"), "binary rvalue includes result type: {mir}");
}

#[test]
fn mir_constant_propagation_folds_checked_arithmetic_and_branches() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-mir-constprop-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn main() {\n  let a = 2 + 3 * 4;\n  let b = a - 4;\n  let cond = b == 10;\n  if cond { print(b); } else { print(0); }\n  let div_zero = 1 / 0;\n  print(div_zero);\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("v0 (i64) = 14"), "2 + 3 * 4 folds to 14: {mir}");
    assert!(mir.contains("v1 (i64) = 10"), "a - 4 propagates and folds to 10: {mir}");
    assert!(mir.contains("v2 (bool) = true"), "b == 10 folds to true: {mir}");
    assert!(!mir.contains("branch "), "the constant branch folds away: {mir}");
    assert!(mir.contains("fn main() -> i64 (1 blocks)"), "the folded arm and its continuation fuse into the entry: {mir}");
    assert!(mir.contains("1 / 0 -> i64"), "division by zero stays unfolded to preserve runtime trap: {mir}");
}

#[test]
fn mir_linear_block_merging_fuses_single_predecessor_successors() {
    // M2.10: a block reachable only by one unconditional jump is fused into
    // its predecessor — bindings in order, the successor's terminator last —
    // so straight-line control flow costs no blocks and no edges. A loop's
    // conditional arm is never fused, and back edges keep pointing at the
    // block that holds the condition.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-mir-merge-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn f(c: bool) -> i64 {\n  var x = 0;\n  if c { x = 1; } else { return 9; }\n  print(x);\n  return x;\n}\nfn sum(n: i64) -> i64 {\n  var total = 0;\n  var i = 0;\n  while i < n { total = total + i; i = i + 1; }\n  return total;\n}\nfn main() { print(f(true)); print(sum(5)); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("fn #0(v0: bool) -> i64 (3 blocks)"), "the returning else arm, the then arm, and the fused continuation survive: {mir}");
    assert!(!mir.contains("goto block 3"), "the continuation fuses into the then arm instead of being jumped to: {mir}");
    assert!(mir.contains("  block 1:\n    v1 (i64) = 1\n    t0 (i64) = v1 (i64)\n    return v1 (i64) @ 5:10"), "the fused block keeps binding order and takes the successor's terminator: {mir}");
    // The while's head is a dedicated block holding only the condition, so a
    // `continue` and the back edge re-check it without re-running the
    // declarations that precede the loop (M2.3).
    assert!(mir.contains("fn #1(v0: i64) -> i64 (4 blocks)"), "declarations, condition, body, and exit blocks survive: {mir}");
    assert!(mir.contains("goto block 1"), "the loop back edge targets the condition block: {mir}");
}

#[test]
fn mir_unreachable_block_elimination_prunes_abandoned_continuations() {
    // M2.10: lowering creates blocks for continuations no path reaches (the
    // join after two returning arms, the fall-through past a return), and
    // constant branch folding orphans the untaken arm. Optimization must
    // prune those blocks while keeping every reachable edge, loop back edges
    // included.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ae-mir-unreachable-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, "fn pick(n: i64) -> i64 {\n  if n > 0 { return 1; } else { return 2; }\n}\nfn main() {\n  var i = 0;\n  while i < 2 { i = i + 1; }\n  print(pick(i));\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("fn #0(v0: i64) -> i64 (3 blocks)"), "only entry and the two returning arms survive: {mir}");
    // The while's head is a dedicated block (M2.3), so main lowers to
    // declarations, condition, body, and exit.
    assert!(mir.contains("fn main() -> i64 (4 blocks)"), "entry, loop condition, body, and exit survive: {mir}");
    assert!(!mir.contains("unreachable"), "abandoned continuations are pruned: {mir}");
    assert!(mir.contains("goto block 1"), "the loop back edge targets the condition block: {mir}");
}

#[test]
fn mir_drop_elision_removes_releases_of_moved_slots() {
    // M2.4: the parser's release lists are conservative — a slot a call
    // already moved out of keeps its scope-end release. The MIR block graph
    // proves the slot dead, so the MIR backend drops that release; the tree
    // backend still emits it and lets the idempotent release no-op on the
    // zeroed slot. Both backends run the program identically.
    let dir = unique_temp_dir("drop_elision");
    let source = dir.join("moves.ae");
    fs::write(&source, "fn take(x: string) -> i64 { return 1; }\nfn main() {\n    let s = \"hello\";\n    print(take(s));\n}\n").unwrap();
    let emit = |mode: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .args(["emit-c", source.to_str().unwrap()])
            .env("AE_CODEGEN", mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
    };
    let mir = emit("mir");
    let tree = emit("tree");
    // Release calls after `int main(`: the MIR backend emits none, because
    // the call consumed `s`; the tree backend emits the conservative one.
    let releases_in_main = |text: &str| -> usize {
        let body = text.split("int main(").nth(1).unwrap_or("");
        body.matches("ae_str_release(&v").count()
    };
    assert_eq!(releases_in_main(&mir), 0, "the moved slot's release is elided: {mir}");
    assert!(releases_in_main(&tree) >= 1, "the tree backend keeps the conservative release: {tree}");
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir_out = String::from_utf8_lossy(&run("mir").stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&run("tree").stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out, "backends disagree");
    assert_eq!(mir_out, "1\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mir_drop_elision_keeps_live_and_unproven_releases() {
    // M2.4: a release the graph cannot prove redundant stays. A binding read
    // across a loop is live on every path, so its scope-end release is kept
    // with proof; a binding moved in one branch only reaches the scope end
    // live on that branch and dropped on the other — no proof decides, so
    // the idempotent release is kept and decides at runtime by finding the
    // slot zeroed or not. The tree backend emits the same releases.
    let dir = unique_temp_dir("drop_kept");
    let source = dir.join("kept.ae");
    fs::write(&source, "fn take(x: string) -> i64 { return 1; }\nfn looped() -> i64 {\n    let s = \"x\";\n    var i = 0;\n    while i < 3 {\n        print(s);\n        i = i + 1;\n    }\n    return len(s);\n}\nfn branchy(c: bool) -> i64 {\n    let s = \"hello\";\n    if c { print(take(s)); }\n    print(\"done\");\n    return 0;\n}\nfn main() {\n    print(looped());\n    print(branchy(true));\n    print(branchy(false));\n}\n").unwrap();
    let emit = |mode: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .args(["emit-c", source.to_str().unwrap()])
            .env("AE_CODEGEN", mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
    };
    let mir = emit("mir");
    let tree = emit("tree");
    // Release calls per function body, addressed by the function's own slot
    // numbering: take releases its parameter, looped's binding is live on
    // every path at its scope end, and branchy's binding is live on one path
    // and moved on the other, so its release survives without proof.
    let releases_in = |text: &str, marker: &str| -> usize {
        let start = text.find(marker).expect(marker);
        let body = &text[start..];
        let end = body.find("\nstatic ").map(|n| body[..n].len()).unwrap_or(body.len());
        body[..end].matches("ae_str_release(&v").count()
    };
    // The markers are the definitions, not the prototypes: a prototype ends
    // in `);` while a definition ends in `) {`.
    assert!(releases_in(&mir, "ae_fn_1(void) {") >= 1, "the loop-carried binding's release is kept: {mir}");
    assert!(releases_in(&mir, "ae_fn_2(bool v0) {") >= 1, "the unproven release is kept: {mir}");
    assert!(releases_in(&tree, "ae_fn_2(bool v0) {") >= 1, "the tree backend emits the same unproven release: {tree}");
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir_out = String::from_utf8_lossy(&run("mir").stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&run("tree").stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out, "backends disagree");
    // x,x,x (loop reads), 1 (len), then branchy(true): 1, done, 0 — the
    // unproven release runs on the moved path as a no-op — and
    // branchy(false): done, 0 — it frees the live binding.
    assert_eq!(mir_out, "x\nx\nx\n1\n1\ndone\n0\ndone\n0\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn self_associated_call_on_plain_enum_resolves_through_the_impl_block() {
    // ADR-0186 inside an enum impl block: `Self::constructor(...)` is the
    // same lookup a bare `Shape::circle(...)` outside the block uses. The
    // generic-enum path (ADR-0196) briefly swallowed this case and refused
    // every plain enum, which only `verify_generated_c.py`'s example corpus
    // caught. The rejection case must keep listing the real methods.
    let dir = unique_temp_dir("self_assoc");
    let source = dir.join("shapes.ae");
    fs::write(&source, "enum Shape {\n    Circle(i64),\n    Rect(i64, i64),\n    Unit,\n}\nimpl Shape {\n    fn circle(r: i64) -> Shape { return Shape::Circle(r); }\n    fn grow(self, by: i64) -> Shape {\n        match self {\n            Shape::Circle(r) => { return Self::circle(r + by); }\n            Shape::Rect(w, h) => { return Shape::Rect(w + by, h + by); }\n            Shape::Unit => { return Shape::Unit; }\n        }\n    }\n    fn describe(self) -> string {\n        match self {\n            Shape::Circle(r) => { return \"circle r=\" + to_string(r); }\n            Shape::Rect(w, h) => { return \"rect \" + to_string(w) + \"x\" + to_string(h); }\n            Shape::Unit => { return \"unit\"; }\n        }\n    }\n}\nfn main() {\n    print(Shape::circle(2).grow(3).describe());\n}\n").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&source).output().unwrap();
    assert!(run.status.success(), "run failed: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "circle r=5\n");

    let bad = dir.join("nosuch.ae");
    fs::write(&bad, "enum E { A }\nimpl E {\n    fn make() -> E { return E::A; }\n    fn go() -> E { return Self::nope(); }\n}\nfn main() { let _ = E::make(); }\n").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&bad).output().unwrap();
    assert!(!rejected.status.success(), "Self::nope must not resolve");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("has no associated function 'nope'"), "diagnostic names the method: {stderr}");
    assert!(stderr.contains("go, make"), "diagnostic lists the impl's methods: {stderr}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concat_reads_fields_and_equality_does_not_consume() {
    // ADR-0184 follow-up: a binary operator changes what the postfix chain
    // means, so the move check must not decide before the operator is seen.
    // A string `+` copies field and element views, comparisons copy nothing,
    // and an owned variable is consumed exactly once even as `s + s`.
    let source = concat!(
        "struct Label { text: string }\n",
        "fn tag(l: Label) -> string {\n",
        "    return l.text + \"/x\";\n",
        "}\n",
        "fn main() {\n",
        "    let l = Label { text: \"field\" };\n",
        "    print(tag(l));\n",
        "    let s = \"abc\";\n",
        "    let t = \"abd\";\n",
        "    let b = s == t;\n",
        "    print(b);\n",
        "    print(s);\n",
        "    let parts = split(\"a-b-c\", \"-\");\n",
        "    let joined = parts[0] + \"!\" + parts[2];\n",
        "    print(joined);\n",
        "}\n",
    );
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "field/x\nfalse\nabc\na!c\n");
}

#[test]
fn generic_borrow_parameters_resolve_at_each_call_site() {
    // `&T` is not a new borrow kind: after substitution it is one of the three
    // read-only borrows the language already has, with the same call syntax.
    let source = r#"fn size_of[T](value: &T) -> i64 { return len(value); }
fn total(values: &vec) -> i64 {
    var sum = 0;
    for i in 0..len(values) { sum = sum + values[i]; }
    return sum;
}
fn forward(values: &vec) -> i64 { return total(&values); }
fn count(parts: &vec[string]) -> i64 { return len(parts); }
fn forward_parts(parts: &vec[string]) -> i64 { return count(&parts); }
fn main() {
    let text = "ae";
    let numbers = vec[10, 20, 30];
    let words = split("alpha,beta,gamma", ",");
    print(size_of[string](text));
    print(size_of[vec](&numbers));
    print(size_of[vec[string]](&words));
    print(forward(&numbers));
    print(forward_parts(&words));
    print(text);
    print(numbers[0]);
    print(words[1]);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\n3\n3\n60\n3\nae\n10\nbeta\n");

    for (source, expected) in [
        ("fn f[T](x: &T) -> i64 { return len(x); } fn main() { print(f[i64](1)); }", "has no read-only borrow"),
        ("fn f[T](x: &T) -> i64 { return len(x); } fn main() { print(f[bool](true)); }", "has no read-only borrow"),
        ("fn f(x: &T) -> i64 { return len(x); } fn main() { print(1); }", "unsupported borrow type"),
        ("fn f[T](x: T) -> &T { return x; } fn main() { print(f[i64](1)); }", "cannot return a borrow"),
        ("struct B[T] { v: &T } fn main() { print(1); }", "expected identifier"),
        // A borrowed string vector is not a borrowed i64 vector.
        ("fn inner(v: &vec) -> i64 { return len(v); } fn outer(v: &vec[string]) -> i64 { return inner(&v); } fn main() { print(1); }", "argument must be vec"),
        // Forwarding must not relax the read-only rule.
        ("fn inner(v: &vec) -> i64 { push(v, 1); return 0; } fn outer(v: &vec) -> i64 { return inner(&v); } fn main() { print(1); }", "push expects a vec"),
        // A view cannot escape the callee, generic or not.
        ("fn f[T](x: &T) -> string { return x; } fn main() { print(f[string](\"a\")); }", "return type must be string"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-generic-borrow-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}

#[test]
fn find_reports_a_byte_offset_or_an_error_value() {
    let source = r#"fn main() {
    let line = "name=ae; mode=fast";
    let eq = find(line, "=");
    match eq { IntResult::Ok(at) => { print(at); } IntResult::Err(reason) => { print(reason); } }
    let mode = find(line, "mode=");
    match mode { IntResult::Ok(at) => { print(at); } IntResult::Err(reason) => { print(reason); } }
    let absent = find(line, "slow");
    match absent { IntResult::Ok(at) => { print(at); } IntResult::Err(reason) => { print(reason); } }
    let empty = find(line, "");
    match empty { IntResult::Ok(at) => { print(at); } IntResult::Err(reason) => { print(reason); } }
    let utf8 = find("中文 abc", "abc");
    match utf8 { IntResult::Ok(at) => { print(at); } IntResult::Err(reason) => { print(reason); } }
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "4\n9\nsubstring not found\n0\n7\n");

    for (source, expected) in [
        ("fn main() { let x = find(1, \"a\"); print(1); }", "expects a string to search in"),
        ("fn main() { let x = find(\"a\", 1); print(1); }", "expects a string to search for"),
        ("fn main() { let find = 1; print(find); }", "built-in name"),
        ("fn main() { find(\"a\", \"b\"); }", "must end with"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-find-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}

#[test]
fn float_arithmetic_prints_a_canonical_form() {
    let source = r#"fn mean(values: &vec) -> f64 {
    var total = 0.0;
    for i in 0..len(values) { total = total + to_f64(values[i]); }
    return total / to_f64(len(values));
}
fn main() {
    let data = vec[2, 4, 4, 4, 5, 5, 7, 9];
    print(mean(&data));
    print(1.0 / 3.0);
    print(0.1 + 0.2);
    print(0.1 + 0.2 == 0.3);
    print(1000.0);
    print(-0.0);
    print(1e16);
    print(0.0001);
    let huge = 1e308 * 10.0;
    print(huge);
    print(huge - huge);
    print(to_string(3.14));
    let whole = to_i64(5.0);
    match whole { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print(reason); } }
    let fraction = to_i64(0.1 + 0.2);
    match fraction { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print(reason); } }
    let not_finite = to_i64(huge);
    match not_finite { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print(reason); } }
    let too_large = to_i64(1e30);
    match too_large { IntResult::Ok(n) => { print(n); } IntResult::Err(reason) => { print(reason); } }
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "5.0\n0.3333333333333333\n0.30000000000000004\nfalse\n1000.0\n-0.0\n1e+16\n1e-4\ninf\nnan\n3.14\n5\nf64 value is not an exact integer\nf64 value is not finite\nf64 value is outside the i64 range\n"
    );
}

#[test]
fn float_exponents_and_fields_resolve_without_mixing() {
    let source = r#"struct Point { x: f64, y: f64 }
enum Reading { Value(f64), Missing }
fn add[T: Add](a: T, b: T) -> T { return a + b; }
fn main() {
    let p = Point { x: 1.5, y: -2.5 };
    print(p.x + p.y);
    let r = Reading::Value(0.25);
    match r { Reading::Value(v) => { print(v * 4.0); } Reading::Missing => { print(0.0); } }
    print(add[f64](1.25, 2.75));
    let maybe = Option[f64]::Some(0.5);
    match maybe { Option::Some(v) => { print(v); } Option::None => { print(0.0); } }
    print(2.5e-3);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "-1.0\n1.0\n4.0\n0.5\n0.0025\n");
}

#[test]
fn float_rejections_keep_conversions_explicit() {
    for (source, expected) in [
        ("fn main() { print(1 + 2.0); }", "mixed i64/f64 arithmetic"),
        ("fn main() { print(5.0 % 2.0); }", "operands only"),
        ("fn main() { if 1.0 { print(1); } }", "boolean"),
        ("fn main() { print(1.0e400); }", "not finite"),
        ("fn main() { print(1.0e-400); }", "underflows"),
        // vec[1.0] became a valid vec[f64] literal (ADR-61); what stays
        // rejected is mixing element types inside one literal.
        ("fn main() { let v = vec[1, 2.0]; }", "does not convert implicitly"),
        ("fn main() { print(to_f64(1.0)); }", "expects an i64"),
        ("fn main() { print(to_i64(1)); }", "expects an f64"),
        ("fn main() { print(to_string(true)); }", "expects an i64 or f64"),
        ("fn main() { let x = vec[1]; print(x[0.5]); }", "index"),
        ("fn main() { let x = 1.0; x = 2; }", "cannot assign"),
    ] {
        let path = std::env::temp_dir().join(format!("ae-float-invalid-{}-{}.ae", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}

#[test]
fn float_runtime_guards_report_instead_of_rounding() {
    for (source, expected) in [
        ("fn main() { print(to_f64(9007199254740993)); }", "would lose precision"),
        ("fn main() { let z = 0.0; print(1.0 / z); }", "f64 division by zero"),
    ] {
        let output = execute(source);
        assert!(!output.status.success(), "{source}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{source}: {}", String::from_utf8_lossy(&output.stderr));
    }
}

#[test]
fn interface_summaries_ignore_function_bodies_but_track_public_signatures() {
    let dir = std::env::temp_dir().join(format!("ae-deps-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    let lib = dir.join("lib.ae");
    let main = dir.join("main.ae");
    let report = || {
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("deps").arg(&main).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
    };

    fs::write(&main, "use \"lib.ae\";\nfn main() { print(twice(3)); }\n").unwrap();
    fs::write(&lib, "pub fn twice(x: i64) -> i64 { return x + x; }\n").unwrap();
    let first = report();
    assert!(first.contains("uses lib.ae"), "{first}");
    assert!(first.contains("exports 1"), "{first}");
    assert!(first.contains("fingerprint "), "{first}");

    // A body-only edit (including a private helper) must not change any hash.
    fs::write(&lib, "fn helper(x: i64) -> i64 { return x + 1; }\npub fn twice(x: i64) -> i64 { let y = x + x; return helper(y) - 1; }\n").unwrap();
    assert_eq!(report(), first, "a body edit changed the interface fingerprint");

    // Changing a public signature changes that module's interface and the
    // program fingerprint, while the dependency graph stays the same.
    fs::write(&main, "use \"lib.ae\";\nfn main() { print(twice(3.0)); }\n").unwrap();
    fs::write(&lib, "pub fn twice(x: f64) -> f64 { return x + x; }\n").unwrap();
    let third = report();
    assert_ne!(third, first, "a signature edit left the fingerprint unchanged");
    assert!(third.contains("uses lib.ae"), "{third}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn deps_reports_the_import_graph_in_load_order() {
    let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("deps")
        .arg("examples/modules/main.ae").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert!(text.contains("module words.ae"), "{text}");
    assert!(text.contains("module report.ae"), "{text}");
    assert!(text.contains("module main.ae"), "{text}");
    assert!(text.contains("  uses words.ae\n  uses report.ae\n"), "{text}");
}

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ae-fmt-test-{}-{}-{}", label, std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn fmt_normalizes_keeps_semantics_and_is_idempotent() {
    let dir = unique_temp_dir("sem");
    let input = dir.join("fmt_target.ae");
    let messy = "fn main(){\n// count\nvar total=0;\nfor i in 0..3 {\ntotal=total+i;\n}\nprint(total); // done\n}\n";
    fs::write(&input, messy).unwrap();
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));
    let before = ae().arg("emit-c").arg(&input).output().unwrap();
    assert!(before.status.success(), "{}", String::from_utf8_lossy(&before.stderr));

    let first = ae().arg("fmt").arg(&input).output().unwrap();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let formatted = fs::read_to_string(&input).unwrap();
    assert_eq!(formatted, "fn main() {\n    // count\n    var total = 0;\n    for i in 0..3 {\n        total = total + i;\n    }\n    print(total); // done\n}\n");

    // A second pass must be a no-op: formatting is idempotent.
    let second = ae().arg("fmt").arg(&input).output().unwrap();
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(fs::read_to_string(&input).unwrap(), formatted);

    // Semantics are unchanged. Byte-equality of the generated C is not the
    // contract: runtime checks embed the source column, and spacing moves
    // columns. What must hold here is that the generated C changes only in
    // embedded numbers, so every differing line matches once digits are
    // ignored. (Token stream and line numbers are covered by the formatter's
    // own unit tests against the compiler lexer.)
    let after = ae().arg("emit-c").arg(&input).output().unwrap();
    let before_c = String::from_utf8_lossy(&before.stdout).replace("\r\n", "\n");
    let after_c = String::from_utf8_lossy(&after.stdout).replace("\r\n", "\n");
    assert_eq!(after_c.lines().count(), before_c.lines().count(), "generated C line count changed");
    let ignore_digits = |line: &str| -> String { line.chars().filter(|c| !c.is_ascii_digit()).collect() };
    for (index, (was, now)) in before_c.lines().zip(after_c.lines()).enumerate() {
        assert_eq!(ignore_digits(was), ignore_digits(now),
            "generated C line {index} changed beyond embedded numbers:\n  before: {was}\n  after:  {now}");
    }

    // And the program still computes the same answer.
    let run = execute(&formatted);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "3\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fmt_adds_the_final_newline_and_reports_no_change_when_clean() {
    let dir = unique_temp_dir("nl");
    let input = dir.join("tidy.ae");
    fs::write(&input, "fn main() { print(1); }").unwrap();
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));
    let result = ae().arg("fmt").arg(&input).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(fs::read_to_string(&input).unwrap(), "fn main() { print(1); }\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fmt_refuses_unlexable_files_and_leaves_them_untouched() {
    let dir = unique_temp_dir("bad");
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));
    for broken in [
        // Unterminated string.
        "fn main() { print(\"open); }\n",
        // A character the language has no token for.
        "fn main() { print(1 @ 2); }\n",
    ] {
        let input = dir.join("broken.ae");
        fs::write(&input, broken).unwrap();
        let result = ae().arg("fmt").arg(&input).output().unwrap();
        assert!(!result.status.success(), "fmt accepted {broken:?}");
        assert!(!String::from_utf8_lossy(&result.stderr).is_empty());
        assert_eq!(fs::read_to_string(&input).unwrap(), broken, "fmt modified a file it refused to format");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fmt_formats_files_that_fail_full_check() {
    // A semantic error does not make whitespace normalization unsafe, and
    // module files have no main of their own, so fmt only requires
    // lexability. A file with an unknown name is still reformatted.
    let dir = unique_temp_dir("semantic");
    let input = dir.join("broken.ae");
    fs::write(&input, "fn main() {\nprint(who_am_i);\n}\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("fmt").arg(&input).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(fs::read_to_string(&input).unwrap(), "fn main() {\n    print(who_am_i);\n}\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fmt_formats_a_module_file_without_a_main() {
    let dir = unique_temp_dir("module");
    let modules = dir.join("modules");
    fs::create_dir(&modules).unwrap();
    for name in ["main.ae", "words.ae", "report.ae"] {
        fs::copy(format!("examples/modules/{name}"), modules.join(name)).unwrap();
    }
    let before = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(modules.join("main.ae")).output().unwrap();
    assert!(before.status.success(), "{}", String::from_utf8_lossy(&before.stderr));

    let result = Command::new(env!("CARGO_BIN_EXE_ae")).arg("fmt").arg(modules.join("report.ae")).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));

    // The whole program still emits the same C up to embedded columns.
    let after = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(modules.join("main.ae")).output().unwrap();
    assert!(after.status.success(), "{}", String::from_utf8_lossy(&after.stderr));
    let ignore_digits = |line: &str| -> String { line.chars().filter(|c| !c.is_ascii_digit()).collect() };
    let strip = |bytes: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(bytes).replace("\r\n", "\n").split('\n')
            .map(ignore_digits).collect()
    };
    assert_eq!(strip(&before.stdout), strip(&after.stdout));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fmt_preserves_comment_and_string_bytes_end_to_end() {
    let dir = unique_temp_dir("bytes");
    let input = dir.join("keep.ae");
    let source = "// header comment\nfn main() {\n    // indented comment\n    print(\"tab\\there quote\\\" done\"); // tail\n    print(1e3);\n    print(007);\n}\n";
    fs::write(&input, source).unwrap();
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));
    let result = ae().arg("fmt").arg(&input).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let formatted = fs::read_to_string(&input).unwrap();
    // Everything but indentation is byte-identical: comments, escapes and
    // literal spellings are re-emitted from the source, not re-created.
    for needle in ["// header comment", "// indented comment", "\"tab\\there quote\\\" done\"); // tail", "1e3", "007"] {
        assert!(formatted.contains(needle), "lost {needle:?} in:\n{formatted}");
    }
    let run = execute(&formatted);
    assert!(run.status.success());
    // The integer literal keeps its value; the float's canonical text is
    // ADR-37's domain and asserted there, not here.
    let out = String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n");
    assert!(out.lines().any(|l| l == "7"), "{out}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_runner_reports_pass_fail_and_panic_with_exit_codes() {
    let dir = unique_temp_dir("runner");
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));

    let passing = dir.join("pass.ae");
    fs::write(&passing, "fn test_add() -> i64 {\n    assert(2 + 2 == 4);\n    return 0;\n}\n\nfn test_len() -> i64 {\n    if len(\"abc\") != 3 { return 1; }\n    return 0;\n}\n").unwrap();
    let output = ae().arg("test").arg(&passing).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "RUN test_add\nPASS test_add\nRUN test_len\nPASS test_len\nok: 2 passed\n");

    // A test returning a non-zero value fails the suite with that code, and
    // tests after it do not run.
    let failing = dir.join("fail.ae");
    fs::write(&failing, "fn test_first() -> i64 {\n    return 0;\n}\n\nfn test_second() -> i64 {\n    return 3;\n}\n\nfn test_third() -> i64 {\n    return 0;\n}\n").unwrap();
    let output = ae().arg("test").arg(&failing).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(stdout.contains("PASS test_first"), "{stdout}");
    assert!(stdout.contains("FAIL test_second"), "{stdout}");
    assert!(!stdout.contains("RUN test_third"), "{stdout}");
    assert!(!stdout.contains("ok:"), "{stdout}");

    // A filter is a substring on the test name; only matching tests run and
    // the ok line counts the filtered set. An empty match is an error, not a
    // green run (ADR-62).
    let filtered = dir.join("filter.ae");
    fs::write(&filtered, "fn test_trim_start() -> i64 {\n    return 0;\n}\n\nfn test_trim_end() -> i64 {\n    return 0;\n}\n\nfn test_join() -> i64 {\n    return 0;\n}\n").unwrap();
    let output = ae().arg("test").arg(&filtered).arg("trim").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "RUN test_trim_start\nPASS test_trim_start\nRUN test_trim_end\nPASS test_trim_end\nok: 2 passed\n");
    let output = ae().arg("test").arg(&filtered).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok: 3 passed"));
    let output = ae().arg("test").arg(&filtered).arg("unicode").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no test functions match filter 'unicode'"), "{}", String::from_utf8_lossy(&output.stderr));

    // A panicking test keeps the existing failure behaviour: location on
    // stderr, exit status 1.
    let panicking = dir.join("panic.ae");
    fs::write(&panicking, "fn test_boom() -> i64 {\n    assert(1 == 2);\n    return 0;\n}\n").unwrap();
    let output = ae().arg("test").arg(&panicking).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("assertion failed"), "{}", String::from_utf8_lossy(&output.stderr));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_runner_refuses_malformed_suites_and_cleans_up() {
    let dir = unique_temp_dir("refuse");
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));
    for broken in [
        // A main is a program, not a suite.
        "fn main() { print(1); }\nfn test_a() -> i64 { return 0; }\n",
        // No test functions at all.
        "fn helper() -> i64 { return 1; }\n",
        // Wrong signature: params.
        "fn test_a(x: i64) -> i64 { return x; }\n",
        // Wrong signature: return type.
        "fn test_a() -> string { return \"s\"; }\n",
        // Generic tests are not supported in v1.
        "fn test_a[T]() -> i64 { return 0; }\n",
    ] {
        let input = dir.join("suite.ae");
        fs::write(&input, broken).unwrap();
        let output = ae().arg("test").arg(&input).output().unwrap();
        assert!(!output.status.success(), "ae test accepted {broken:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).is_empty());
        // The staged copy next to the input must be gone on every path.
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".ae-test-"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_runner_works_with_modules_and_finds_failures_there() {
    // Tests in the entry file may call helpers from used modules; module
    // files stay plain modules (their tests are not collected in v1).
    let dir = unique_temp_dir("mods");
    let modules = dir.join("modules");
    fs::create_dir(&modules).unwrap();
    fs::write(modules.join("lib.ae"), "pub fn twice(x: i64) -> i64 { return x + x; }\n").unwrap();
    fs::write(modules.join("main_t.ae"),
        "use \"lib.ae\";\n\nfn test_twice() -> i64 {\n    if twice(21) != 42 { return 1; }\n    return 0;\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("test").arg(modules.join("main_t.ae")).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "RUN test_twice\nPASS test_twice\nok: 1 passed\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn map_grows_reuses_removed_slots_and_keeps_every_survivor() {
    // ADR-56 halved the load factor, so the table grows at a different point and
    // tombstones interact with growth differently than before. Insert enough keys
    // to grow several times, delete every other one, then insert a second family:
    // survivors must keep their values, removed keys must stay absent, and a slot
    // reused from a tombstone must not shadow a live key.
    let source = r#"
fn main() {
    var m = map_new();
    for i in 0..5000 { let k = "k" + to_string(i); let n = map_inc(m, k, i); }
    print(len(m));
    var removed = 0;
    for i in 0..5000 {
        if i % 2 == 0 {
            let k = "k" + to_string(i);
            if map_remove(m, k) { removed = removed + 1; }
        }
    }
    print(removed);
    print(len(m));
    for i in 0..5000 { let k = "n" + to_string(i); let n = map_inc(m, k, 1); }
    print(len(m));
    var bad = 0;
    for i in 0..5000 {
        let k = "k" + to_string(i);
        let c = map_get(m, k);
        match c {
            IntResult::Ok(n) => { if i % 2 == 0 || n != i { bad = bad + 1; } }
            IntResult::Err(reason) => { if i % 2 == 1 { bad = bad + 1; } }
        }
    }
    for i in 0..5000 {
        let k = "n" + to_string(i);
        if !map_has(m, k) { bad = bad + 1; }
    }
    print(bad);
}
"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "5000\n2500\n2500\n7500\n0\n"
    );
}

#[test]
fn map_inc_counts_and_returns_the_new_value() {
    let output = execute(
        "fn main() {\n    var m = map_new();\n    let first = map_inc(m, \"k\", 1);\n    let second = map_inc(m, \"k\", 1);\n    let big = map_inc(m, \"k\", 40);\n    let negative = map_inc(m, \"other\", 0 - 3);\n    print(first);\n    print(second);\n    print(big);\n    print(negative);\n    print(len(m));\n    let seen = map_get(m, \"k\");\n    match seen {\n        IntResult::Ok(n) => { print(n); }\n        IntResult::Err(reason) => { print(\"missing\"); }\n    }\n}");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "1\n2\n42\n-3\n2\n42\n");
}

#[test]
fn map_inc_rejects_bad_shapes_and_moved_maps() {
    let rejects = [
        ("fn main() { var m = map_new(); let n = map_inc(m, 5, 1); print(n); }", "string key"),
        ("fn main() { var m = map_new(); let n = map_inc(m, \"k\", \"x\"); print(n); }", "i64 delta"),
        ("fn main() { var m = map_new(); let b = m; let n = map_inc(m, \"k\", 1); print(n); }", "moved map"),
    ];
    for (source, needle) in rejects {
        let message = check_rejects(source);
        // A moved map is usually caught by the general move rule before the
        // builtin's own check runs; either rejection is correct.
        assert!(message.contains(needle) || message.contains("was moved"), "{source:?}: {message}");
    }
}

#[test]
fn map_inc_works_on_struct_fields_like_the_other_map_builtins() {
    let output = execute(
        "struct Bag { counts: map[string, i64], tag: i64 }\nfn main() {\n    var b = Bag { counts: map_new(), tag: 1 };\n    let first = map_inc(b.counts, \"a\", 2);\n    let now = map_inc(b.counts, \"a\", 2);\n    print(first);\n    print(now);\n    print(len(b.counts));\n}");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "2\n4\n1\n");
}

#[test]
fn trait_dispatch_resolves_per_type_and_matches_the_hand_written_call() {
    let output = execute(
        "trait Area {\n    fn area(shape: T) -> i64;\n}\n\nimpl Area for i64 {\n    fn area(shape: i64) -> i64 { return shape * shape; }\n}\n\nstruct Rect { w: i64, h: i64 }\n\nimpl Area for Rect {\n    fn area(shape: Rect) -> i64 { return shape.w * shape.h; }\n}\n\nfn report[T: Area](shape: T) -> string {\n    return \"area=\" + to_string(area(shape));\n}\n\nfn rect_area(shape: Rect) -> i64 { return shape.w * shape.h; }\n\nfn main() {\n    print(report[i64](7));\n    print(report[Rect](Rect { w: 3, h: 4 }));\n    print(rect_area(Rect { w: 3, h: 4 }));\n}");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "area=49\narea=12\n12\n");
}

#[test]
fn trait_rules_are_enforced() {
    let rejects = [
        // No impl for the argument the call site chose.
        ("trait Area {\n    fn area(shape: T) -> i64;\n}\nimpl Area for i64 {\n    fn area(shape: i64) -> i64 { return shape; }\n}\nfn report[T: Area](shape: T) -> i64 {\n    return area(shape);\n}\nfn main() {\n    print(report[string](\"x\"));\n}", "no impl of trait 'Area' for 'string'"),
        // The body calls the method but the parameter is not constrained.
        ("fn report[T](shape: T) -> i64 {\n    return area(shape);\n}\nfn main() {\n    print(report[i64](1));\n}", "unknown function 'area'"),
        // Two impls for the same (trait, type) pair.
        ("trait Area {\n    fn area(shape: T) -> i64;\n}\nimpl Area for i64 {\n    fn area(shape: i64) -> i64 { return shape; }\n}\nimpl Area for i64 {\n    fn area(shape: i64) -> i64 { return 0; }\n}\nfn main() {\n    print(1);\n}", "duplicate impl of 'Area' for 'i64'"),
        // The impl's signature disagrees with the trait.
        ("trait Area {\n    fn area(shape: T) -> i64;\n}\nimpl Area for i64 {\n    fn area(shape: bool) -> i64 { return 1; }\n}\nfn main() {\n    print(1);\n}", "impl method 'area' must take (i64)"),
        // The bound names something that is not a trait.
        ("fn report[T: Area](shape: T) -> i64 {\n    return 1;\n}\nfn main() {\n    print(report[i64](1));\n}", "unknown constraint"),
        // The impl names an unknown trait.
        ("impl Area for i64 {\n    fn area(shape: i64) -> i64 { return 1; }\n}\nfn main() {\n    print(1);\n}", "impl names unknown trait 'Area'"),
        // The impl invents a method the trait never declared.
        ("trait Area {\n    fn area(shape: T) -> i64;\n}\nimpl Area for i64 {\n    fn width(shape: i64) -> i64 { return 1; }\n}\nfn main() {\n    print(1);\n}", "trait 'Area' has no method 'width'"),
        // The impl's return type disagrees.
        ("trait Area {\n    fn area(shape: T) -> i64;\n}\nimpl Area for i64 {\n    fn area(shape: i64) -> bool { return true; }\n}\nfn main() {\n    print(1);\n}", "impl method 'area' must return i64"),
    ];
    for (source, needle) in rejects {
        let message = check_rejects(source);
        assert!(message.contains(needle), "{source:?}: {message}");
    }
}

#[test]
fn trait_impl_lives_with_its_trait_or_type_and_third_files_are_refused() {
    // Coherence, v1 shape: an impl belongs to the file that declares the
    // trait or the type; an unrelated third file cannot provide the impl.
    let dir = unique_temp_dir("traitmod");
    let modules = dir.join("modules");
    fs::create_dir(&modules).unwrap();
    fs::write(modules.join("shape.ae"),
        "pub trait Area {\n    fn area(shape: T) -> i64;\n}\n\npub struct Rect { w: i64, h: i64 }\n\nimpl Area for Rect {\n    fn area(shape: Rect) -> i64 { return shape.w * shape.h; }\n}\n").unwrap();
    fs::write(modules.join("main_t.ae"),
        "use \"shape.ae\";\n\nfn report[T: Area](shape: T) -> i64 {\n    return area(shape);\n}\n\nfn main() {\n    print(report[Rect](Rect { w: 3, h: 4 }));\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(modules.join("main_t.ae")).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "12\n");

    // A third file that declares neither the trait nor the type may not
    // write the impl, even though both are exported.
    fs::write(modules.join("third.ae"),
        "use \"shape.ae\";\n\nimpl Area for Rect {\n    fn area(shape: Rect) -> i64 { return 0; }\n}\n").unwrap();
    fs::write(modules.join("main_bad.ae"),
        "use \"third.ae\";\nfn main() {\n    print(1);\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(modules.join("main_bad.ae")).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("impl of 'Area' for 'struct"), "{}", String::from_utf8_lossy(&output.stderr));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn check_json_reports_structured_diagnostics_or_an_empty_array() {
    let ae = || Command::new(env!("CARGO_BIN_EXE_ae"));

    let dir = unique_temp_dir("checkjson");
    let good = dir.join("good.ae");
    fs::write(&good, "fn main() { print(1); }\n").unwrap();
    let output = ae().arg("check").arg("--json").arg(&good).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n").trim(), "[]");
    // The flag is also accepted after the path; both spellings are documented.
    let output = ae().arg("check").arg(&good).arg("--json").output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n").trim(), "[]");

    let bad = dir.join("bad.ae");
    fs::write(&bad, "fn main() { print(who_am_i); }\n").unwrap();
    let output = ae().arg("check").arg("--json").arg(&bad).output().unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(stdout.trim_start().starts_with('['), "{stdout}");
    assert!(stdout.contains("\"file\""), "{stdout}");
    assert!(stdout.contains("\"line\":"), "{stdout}");
    assert!(stdout.contains("\"column\":"), "{stdout}");
    assert!(stdout.contains("unknown variable"), "{stdout}");
    assert!(stdout.trim_end().ends_with(']'), "{stdout}");
    // Windows paths carry backslashes, so the escaping must survive them; on
    // Unix the separator is `/` and the same assertion would be vacuous, so
    // the path itself is the thing worth checking.
    #[cfg(windows)]
    assert!(stdout.contains("\\\\"), "separators not escaped: {stdout}");
    #[cfg(unix)]
    assert!(stdout.contains("\"file\":\"/"), "absolute unix path missing: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn check_json_recovers_impl_methods_and_functions_in_source_order() {
    let dir = unique_temp_dir("json-impl-recovery");
    let path = dir.join("bad.ae");
    fs::write(&path, "trait Area { fn area(x: T) -> i64; fn width(x: T) -> i64; }\nfn before() -> i64 { return missing_before; }\nimpl Area for i64 {\n fn area(x: i64) -> i64 { return missing_area; }\n fn width(x: i64) -> i64 { return missing_width; }\n}\nfn main() { print(missing_main); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    let positions: Vec<_> = ["missing_before", "missing_area", "missing_width", "missing_main"].iter()
        .map(|name| json.find(name).unwrap_or_else(|| panic!("missing {name}: {json}"))).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{json}");
    assert_eq!(json.matches("\"message\":").count(), 4, "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_recovers_independent_function_body_errors() {
    let dir = unique_temp_dir("json-multiple-bodies");
    let path = dir.join("bad.ae");
    fs::write(&path, "fn first() -> i64 { return missing_a; }\nfn second() -> i64 { return missing_b; }\nfn main() { print(missing_c); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 3, "{json}");
    for name in ["missing_a", "missing_b", "missing_c"] { assert!(json.contains(name), "{json}"); }
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("missing_a") && !text.contains("missing_b"), "{text}");
    fs::write(&path, "fn identity[T](x: T) -> T { return x; }\nfn first() -> i64 { return identity[i64](true); }\nfn good() -> i64 { return identity[i64](7); }\nfn last() -> i64 { return missing_last; }\nfn main() {}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 2, "{json}");
    assert!(json.contains("missing_last"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parser_diagnostic_retains_current_token_range() {
    let dir = unique_temp_dir("diagnostic-range");
    let path = dir.join("bad.ae");
    let source = "// 中文\nfn main() { let value = ; }\n";
    fs::write(&path, source).unwrap();
    let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
    let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected a parser diagnostic") };
    let span = diagnostic.primary.unwrap().span;
    let offset = source.find(';').unwrap();
    assert_eq!(span.range, offset..offset + 1);
    assert_eq!((span.line, span.column), (2, 25));
    let source = "fn main() { print(unknown_name); }\n";
    fs::write(&path, source).unwrap();
    let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
    let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected a parser diagnostic") };
    let span = diagnostic.primary.unwrap().span;
    let offset = source.find("unknown_name").unwrap();
    assert_eq!(&source[span.range], "unknown_name");
    assert_eq!((span.line, span.column), (1, offset + 1));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn duplicate_binding_errors_label_the_earlier_declaration() {
    let dir = unique_temp_dir("duplicate-secondary");
    let path = dir.join("case.ae");
    for (source, marker) in [
        ("// 中文\nfn main() { let value = 1; let value = 2; }", "value"),
        ("fn f(value: i64) -> i64 { let value = 2; return value; } fn main() {}", "value"),
    ] {
        fs::write(&path, source).unwrap();
        let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
        let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected one diagnostic") };
        assert_eq!(diagnostic.code.as_deref(), Some("E0104"));
        assert_eq!(diagnostic.secondary.len(), 1);
        let start = source.find(marker).unwrap();
        assert_eq!(diagnostic.secondary[0].span.range, start..start + marker.len());
        assert_eq!(diagnostic.secondary[0].message.as_deref(), Some("binding declared here"));
    }
    fs::write(&path, "fn main() { if true { let value = 1; } if true { let value = 2; } }").unwrap();
    assert!(ae_compiler::frontend::check_path_diagnostic(&path).is_ok());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn move_errors_name_the_rule_without_claiming_a_move_site() {
    let dir = unique_temp_dir("move-secondary");
    let path = dir.join("case.ae");
    for (source, marker) in [
        ("fn main() { let original = \"x\"; let other = original; print(original); }", "original"),
        ("fn f(item: string) -> i64 { let other = item; print(item); return 0; } fn main() {}", "item"),
        ("fn main() { let original = \"x\"; if true { let other = original; } print(original); }", "original"),
        ("fn f[T](item: T) -> T { let other = item; return item; } fn main() { let result = f[string](\"x\"); print(result); }", "item"),
    ] {
        fs::write(&path, source).unwrap();
        let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
        let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected one diagnostic") };
        assert_eq!(diagnostic.code.as_deref(), Some("E0102"));
        assert_eq!(diagnostic.secondary.len(), 1);
        let label = &diagnostic.secondary[0];
        let start = source.find(marker).unwrap();
        assert_eq!(label.span.range, start..start + marker.len());
        assert_eq!(label.message.as_deref(), Some("binding declared here"));
        // The diagnostic names the rule and its remedies but must not claim to
        // know where the move happened: only the declaration is located, so no
        // label may say "moved here".
        assert_eq!(diagnostic.suggestions.len(), 1);
        assert!(diagnostic.suggestions[0].starts_with("Owned values move on assignment or when passed by value"));
        assert!(diagnostic.secondary.iter().all(|label| label.message.as_deref() != Some("moved here")));
    }
    fs::write(&path, "fn main() { var original = \"x\"; let other = original; original = \"y\"; print(original); }").unwrap();
    assert!(ae_compiler::frontend::check_path_diagnostic(&path).is_ok());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn immutable_binding_diagnostics_include_only_known_declarations() {
    let dir = unique_temp_dir("diagnostic-secondary");
    let path = dir.join("case.ae");
    for (source, marker) in [
        ("// 中文\nfn main() { let value = 1; value = 2; }", "value"),
        ("fn f(parameter: i64) -> i64 { parameter = 2; return parameter; } fn main() {}", "parameter"),
        ("fn f[T](item: T) -> T { item = item; return item; } fn main() { print(f[i64](1)); }", "item"),
    ] {
        fs::write(&path, source).unwrap();
        let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
        let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected one diagnostic") };
        assert_eq!(diagnostic.code.as_deref(), Some("E0103"));
        assert_eq!(diagnostic.secondary.len(), 1);
        let label = &diagnostic.secondary[0];
        let start = source.find(marker).unwrap();
        assert_eq!(label.span.range, start..start + marker.len());
        assert_eq!(label.message.as_deref(), Some("binding declared here"));
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
        assert!(!output.status.success());
        let json = String::from_utf8(output.stdout).unwrap();
        assert!(json.contains(&format!("\"byte_start\":{start},\"byte_end\":{},\"message\":\"binding declared here\"", start + marker.len())), "{json}");
    }
    fs::write(&path, "fn main() { for i in 0..2 { i = 1; } }").unwrap();
    let error = ae_compiler::frontend::check_path_diagnostic(&path).unwrap_err();
    let ae_compiler::frontend::FrontendError::Diagnostic(diagnostic) = error else { panic!("expected one diagnostic") };
    assert_eq!(diagnostic.code.as_deref(), Some("E0103"));
    assert!(diagnostic.secondary.is_empty());
    assert!(diagnostic.suggestions.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_reports_only_applicable_suggestions() {
    let dir = unique_temp_dir("json-suggestions");
    let path = dir.join("case.ae");
    fs::write(&path, "fn main() { let n = 1; n = 2; }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"suggestions\":[\"If mutation is intended, declare the binding with `var`.\"]"), "{json}");
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!String::from_utf8_lossy(&output.stderr).contains("If mutation is intended"));
    fs::write(&path, "fn main() { print(\"bad\\q\"); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"code\":\"E0002\""), "{json}");
    assert!(json.contains("\"suggestions\":[\"Use a supported escape, or escape a literal backslash as \\\\\\\\.\"]"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_assigns_stable_codes_to_common_semantic_errors() {
    let dir = unique_temp_dir("json-semantic-codes");
    let path = dir.join("case.ae");
    for (source, code) in [
        ("fn main() { print(missing); }", "E0101"),
        ("fn main() { let a = \"x\"; let b = a; print(a); }", "E0102"),
        ("fn main() { let n = 1; n = 2; }", "E0103"),
        ("fn main() { let n = 1; let n = 2; }", "E0104"),
    ] {
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
        assert!(!output.status.success(), "{source}");
        let json = String::from_utf8(output.stdout).unwrap();
        assert!(json.contains(&format!("\"code\":\"{code}\"")), "{source}: {json}");
    }
    fs::write(&path, "fn main() { var n = 1; n = 2; print(n); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_reports_eof_range_and_unknown_ranges_as_null() {
    let dir = unique_temp_dir("json-range-boundaries");
    let path = dir.join("eof.ae");
    let source = "// 中文\nfn main() {";
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains(&format!("\"byte_start\":{},\"byte_end\":{}", source.len(), source.len())), "{json}");
    let missing = dir.join("missing.ae");
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&missing).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"byte_start\":null,\"byte_end\":null"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_collects_lexical_errors_across_sibling_imports() {
    let dir = unique_temp_dir("json-import-lex");
    fs::create_dir(dir.join("sub")).unwrap();
    let entry = dir.join("main.ae");
    fs::write(&entry, "use \"left.ae\";\nuse \"right.ae\";\nfn main() {}\n").unwrap();
    fs::write(dir.join("left.ae"), "use \"shared.ae\";\n").unwrap();
    fs::write(dir.join("right.ae"), "use \"sub/../shared.ae\";\nuse \"other.ae\";\n").unwrap();
    fs::write(dir.join("shared.ae"), "@\n").unwrap();
    fs::write(dir.join("other.ae"), "`\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 2, "{json}");
    assert!(json.find("shared.ae").unwrap() < json.find("other.ae").unwrap(), "{json}");
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&entry).output().unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(text.contains("shared.ae") && !text.contains("other.ae"), "{text}");
    // An unreadable sibling is reported without suppressing later branches.
    fs::write(dir.join("right.ae"), "use \"missing.ae\";\nuse \"other.ae\";\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 3, "{json}");
    assert!(json.find("shared.ae").unwrap() < json.find("missing.ae").unwrap(), "{json}");
    assert!(json.find("missing.ae").unwrap() < json.find("other.ae").unwrap(), "{json}");
    assert!(json.contains("cannot open"), "{json}");
    // A malformed import header and an import cycle are branch-local errors.
    fs::write(dir.join("malformed.ae"), "use bad;\n").unwrap();
    fs::write(dir.join("cycle.ae"), "use \"cycle.ae\";\n").unwrap();
    fs::write(dir.join("right.ae"), "use \"malformed.ae\";\nuse \"cycle.ae\";\nuse \"other.ae\";\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 4, "{json}");
    assert!(json.contains("expected a string path") && json.contains("circular import") && json.contains("other.ae"), "{json}");
    fs::write(dir.join("right.ae"), "use \"other.ae\";\n").unwrap();
    fs::write(dir.join("shared.ae"), "@\n".repeat(20)).unwrap();
    fs::write(dir.join("other.ae"), "~\n".repeat(20)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 32, "{json}");
    // A semantic failure in the first branch must not be replaced by a
    // later branch's lexical failure discovered by supplementary scanning.
    fs::write(dir.join("shared.ae"), "fn bad() -> i64 { return unknown_name; }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("unknown_name") && !json.contains("unexpected character"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_reports_multiple_lexical_errors_without_accepting_program() {
    let dir = unique_temp_dir("json-multiple-lex");
    let path = dir.join("bad.ae");
    fs::write(&path, "// 注释中的 @ 不报错\n@ 中 ~\nfn main() {}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"message\":").count(), 3, "{json}");
    for (column, character) in [(1, "@"), (3, "中"), (5, "~")] {
        assert!(json.contains(&format!("\"line\":2,\"column\":{column},\"message\":\"unexpected character '{character}'\"")), "{json}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("2:1: unexpected character '@'"), "{stderr}");
    assert!(!stderr.contains("unexpected character '中'"), "{stderr}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_json_preserves_diagnostic_location_in_imported_file() {
    let dir = unique_temp_dir("json-lex");
    #[cfg(windows)]
    let name = "模块.ae";
    #[cfg(not(windows))]
    let name = "模块: part.ae";
    let imported = dir.join(name);
    let entry = dir.join("main.ae");
    let source = "// 中文\n  @\n";
    fs::write(&imported, source).unwrap();
    fs::write(&entry, format!("use \"{name}\";\nfn main() {{}}\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg("--json").arg(&entry).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let escaped = imported.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let start = source.find('@').unwrap();
    let end = start + 1;
    let expected = format!("[{{\"file\":\"{escaped}\",\"line\":2,\"column\":3,\"message\":\"unexpected character '@'\",\"byte_start\":{start},\"byte_end\":{end},\"code\":\"E0001\",\"suggestions\":[],\"secondary\":[]}}]");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    let source = "// 中文\nuse nope;\n";
    fs::write(&imported, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg("--json").arg(&entry).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let start = source.find("nope").unwrap();
    let end = start + "nope".len();
    let expected = format!("[{{\"file\":\"{escaped}\",\"line\":2,\"column\":5,\"message\":\"expected a string path after 'use'\",\"byte_start\":{start},\"byte_end\":{end},\"code\":null,\"suggestions\":[],\"secondary\":[]}}]");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    let source = "pub fn broken() -> i64 { return nope; }\n";
    fs::write(&imported, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg("--json").arg(&entry).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let column = source.find("nope").unwrap() + 1;
    let start = column - 1;
    let end = start + "nope".len();
    let expected = format!("[{{\"file\":\"{escaped}\",\"line\":1,\"column\":{column},\"message\":\"unknown variable 'nope'\",\"byte_start\":{start},\"byte_end\":{end},\"code\":\"E0101\",\"suggestions\":[],\"secondary\":[]}}]");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn json_spans_pin_utf8_byte_offsets_and_char_columns_after_multibyte_text() {
    let dir = unique_temp_dir("json-multibyte-policy");
    let file = dir.join("main.ae");
    // `é` is 2 UTF-8 bytes and each of `世界` is 3 bytes, but one char each:
    // byte_start/byte_end must be absolute UTF-8 byte offsets while columns
    // stay 1-based char positions, so an editor and a terminal both point at
    // the same token (M0.1.03).
    let source = "fn main() {\n    let s = \"héllo世界\"; print(nope);\n}\n";
    fs::write(&file, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check").arg("--json").arg(&file).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let start = source.find("nope").unwrap();
    let line_start = source[..start].rfind('\n').unwrap() + 1;
    let column = source[line_start..start].chars().count() + 1;
    let escaped = file.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let expected = format!("[{{\"file\":\"{escaped}\",\"line\":2,\"column\":{column},\"message\":\"unknown variable 'nope'\",\"byte_start\":{start},\"byte_end\":{},\"code\":\"E0101\",\"suggestions\":[],\"secondary\":[]}}]", start + "nope".len());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn check_render_underlines_tokens_after_tabs_and_wide_characters() {
    // M0.1.04: `check --render` pins the terminal display policy — a tab
    // advances to the next multiple of 4 cells and a CJK character occupies
    // two — so the editor (1-based char columns) and the terminal (cells)
    // point at the same token.
    let dir = unique_temp_dir("check-render-policy");
    let file = dir.join("main.ae");
    let source = "fn main() {\n\tlet s = \"中文\"; print(nope);\n}\n";
    fs::write(&file, source).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let render = || Command::new(cli).arg("check").arg("--render").arg(&file).output().unwrap();

    let output = render();
    assert!(!output.status.success() && output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("2:22: unknown variable 'nope'"), "{stderr}");
    let line2_start = "fn main() {\n".len();
    let line2_end = line2_start + source[line2_start..].find('\n').unwrap();
    assert!(stderr.contains(&format!(" 2 | {}", &source[line2_start..line2_end])), "{stderr}");
    // tab→4, `let s = "`→13, `中文`→17, `"; print(`→26 cells; `nope` is 4.
    assert!(stderr.contains(&format!("   | {}^^^^", " ".repeat(26))), "{stderr}");

    let clean = dir.join("clean.ae");
    fs::write(&clean, "fn main() { print(7); }\n").unwrap();
    let ok = Command::new(cli).arg("check").arg("--render").arg(&clean).output().unwrap();
    assert!(ok.status.success() && ok.stdout.is_empty() && ok.stderr.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn numeric_lexical_errors_recover_in_json_without_accepting_program() {
    let dir = unique_temp_dir("json-numeric-recovery");
    let path = dir.join("bad.ae");
    fs::write(&path, "// unicode: 中文\nfn main() { print(9223372036854775808); @ print(1e999); print(1e-999); }\n").unwrap();
    let json = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!json.status.success());
    assert!(json.stderr.is_empty());
    let text = String::from_utf8(json.stdout).unwrap();
    // The repaired body's own parse error trails the lexical diagnostics:
    // the `@` left an empty argument position in `print(, ...)`, which the
    // body replay reports after the lexical chain (M0.5 completion).
    let positions: Vec<_> = ["E0005", "E0001", "E0007", "E0008", "expected expression"].iter().map(|code| text.find(code).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    assert_eq!(text.matches("\"code\":").count(), 5, "{text}");
    let plain = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!plain.status.success());
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(stderr.contains("integer literal exceeds i64 maximum"), "{stderr}");
    assert!(!stderr.contains("unexpected character"), "{stderr}");
    let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&path).output().unwrap();
    assert!(!generated.status.success());
    assert!(generated.stdout.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn invalid_escapes_in_closed_strings_allow_json_lexical_recovery() {
    let dir = unique_temp_dir("json-string-recovery");
    let path = dir.join("bad.ae");
    fs::write(&path, "fn main() { print(\"bad\\q and \\z\"); @ }\n").unwrap();
    let json = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!json.status.success());
    assert!(json.stderr.is_empty());
    let text = String::from_utf8(json.stdout).unwrap();
    assert_eq!(text.matches("\"code\":\"E0002\"").count(), 2, "{text}");
    assert_eq!(text.matches("\"code\":\"E0001\"").count(), 1, "{text}");
    let plain = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stderr).contains("unsupported string escape"));
    let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&path).output().unwrap();
    assert!(!generated.status.success());
    assert!(generated.stdout.is_empty());
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn check_json_recovers_independent_unknown_prints_in_one_body() {
    let dir = unique_temp_dir("json-multiple-prints");
    let path = dir.join("bad.ae");
    fs::write(&path, "fn main() { print(first_missing); print(second_missing); print(third_missing); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    let positions: Vec<_> = ["first_missing", "second_missing", "third_missing"].iter().map(|name| json.find(name).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
    assert_eq!(json.matches("\"code\":\"E0101\"").count(), 3, "{json}");
    let plain = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
    assert!(!plain.status.success());
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(stderr.contains("first_missing") && !stderr.contains("second_missing"), "{stderr}");
    let generated = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-c").arg(&path).output().unwrap();
    assert!(!generated.status.success() && generated.stdout.is_empty());
    fs::write(&path, "fn main() { print(first_missing + 1); print(second_missing); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("first_missing") && !json.contains("second_missing"), "{json}");
    let many = (0..40).map(|i| format!("print(missing_{i}); ")).collect::<String>();
    fs::write(&path, format!("fn main() {{ {many} }}\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"code\":\"E0101\"").count(), 32, "{json}");
    assert!(!json.contains("missing_32"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn check_json_recovers_independent_unknown_conditions() {
    let dir = unique_temp_dir("json-multiple-conditions");
    let path = dir.join("bad.ae");
    fs::write(&path, "fn main() { assert(first_missing); if second_missing { print(third_missing); } while fourth_missing { assert(fifth_missing); } }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    let positions: Vec<_> = ["first_missing", "second_missing", "third_missing", "fourth_missing", "fifth_missing"].iter().map(|name| json.find(name).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
    assert_eq!(json.matches("\"code\":\"E0101\"").count(), 5, "{json}");
    fs::write(&path, "fn main() { assert(first_missing && true); print(second_missing); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&path).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("first_missing") && !json.contains("second_missing"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn json_lexical_recovery_follows_verified_import_prefixes() {
    let dir = unique_temp_dir("json-lex-prefix");
    let entry = dir.join("main.ae");
    fs::write(&entry, "use \"child.ae\";\nfn main() { print(\"bad\\q\"); }\n").unwrap();
    fs::write(dir.join("child.ae"), "use \"nested.ae\";\n@\n").unwrap();
    fs::write(dir.join("nested.ae"), "`\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    assert!(!output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    let positions: Vec<_> = ["E0002", "E0001", "nested.ae"].iter().map(|item| json.find(item).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
    assert_eq!(json.matches("\"code\":").count(), 3, "{json}");
    // A semicolon inside the malformed string is not an import boundary.
    fs::write(&entry, "use \"child.ae\";\nfn main() { print(\"text; bad\\q\"); }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("child.ae") && json.contains("nested.ae"), "{json}");
    // A malformed import before the lexical error is not guessed or traversed.
    fs::write(&entry, "use nope;\n@\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&entry).output().unwrap();
    let json = String::from_utf8(output.stdout).unwrap();
    assert_eq!(json.matches("\"code\":").count(), 1, "{json}");
    assert!(!json.contains("child.ae"), "{json}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mir_interpreter_executes_programs_and_tests_with_ub_detection() {
    let dir = unique_temp_dir("mir-interp");
    let prog = dir.join("prog.ae");
    fs::write(
        &prog,
        "struct Acc { n: i64 }\nimpl Acc {\n  fn add(&mut self, d: i64) -> i64 { self.n = self.n + d; return self.n; }\n  fn get(&self) -> i64 { return self.n; }\n}\nenum Choice { Val(i64), Empty }\nfn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }\nfn main() {\n  var a = Acc { n: 10 };\n  let ignored = a.add(5);\n  assert(ignored == 15);\n  let c = Choice::Val(a.get());\n  match c {\n    Choice::Val(v) => {\n      let inc = |z: i64| -> i64 { return z + 1; };\n      print(apply(|z: i64| -> i64 { return z + 1; }, inc(v)));\n    }\n    Choice::Empty => { print(0); }\n  }\n}\n",
    ).unwrap();
    let interp_out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", prog.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp_out.status.success(), "{}", String::from_utf8_lossy(&interp_out.stderr));
    assert_eq!(String::from_utf8_lossy(&interp_out.stdout).replace("\r\n", "\n"), "17\n");

    let c_out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", prog.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(c_out.status.success(), "{}", String::from_utf8_lossy(&c_out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&interp_out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&c_out.stdout).replace("\r\n", "\n")
    );

    let tests_file = dir.join("suite.ae");
    fs::write(
        &tests_file,
        "fn test_pass_arith() -> i64 {\n  var v = vec[1, 2, 3];\n  push(v, 4);\n  assert(len(v) == 4 && v[3] == 4);\n  return 0;\n}\nfn test_fail_overflow() -> i64 {\n  let x = 9223372036854775807;\n  let y = x + 1;\n  assert(y > 0);\n  return 0;\n}\n",
    ).unwrap();
    let pass_only = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["test", tests_file.to_str().unwrap(), "pass_arith", "--interp"])
        .output()
        .unwrap();
    assert!(pass_only.status.success(), "{}", String::from_utf8_lossy(&pass_only.stderr));
    let pass_stdout = String::from_utf8_lossy(&pass_only.stdout);
    assert!(pass_stdout.contains("PASS test_pass_arith"), "{pass_stdout}");
    assert!(pass_stdout.contains("ok: 1 passed"), "{pass_stdout}");

    let all_tests = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["test", tests_file.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(!all_tests.status.success());
    let stdout = String::from_utf8_lossy(&all_tests.stdout);
    let stderr = String::from_utf8_lossy(&all_tests.stderr);
    assert!(stdout.contains("RUN test_fail_overflow"), "{stdout}");
    assert!(stderr.contains("integer overflow"), "{stderr}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn three_way_differential_agreement_between_debug_release_and_interp() {
    let suite = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run");
    for name in ["closures.ae", "methods.ae", "enum_generic_impls.ae"] {
        let path = suite.join(name);
        let p = path.to_str().unwrap();
        let debug_out = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", p]).output().unwrap();
        let release_out = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", p, "--release"]).output().unwrap();
        let interp_out = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", p, "--interp"]).output().unwrap();
        assert!(debug_out.status.success(), "{name} debug failed: {}", String::from_utf8_lossy(&debug_out.stderr));
        assert!(release_out.status.success(), "{name} release failed: {}", String::from_utf8_lossy(&release_out.stderr));
        assert!(interp_out.status.success(), "{name} interp failed: {}", String::from_utf8_lossy(&interp_out.stderr));
        let d = String::from_utf8_lossy(&debug_out.stdout).replace("\r\n", "\n");
        let r = String::from_utf8_lossy(&release_out.stdout).replace("\r\n", "\n");
        let i = String::from_utf8_lossy(&interp_out.stdout).replace("\r\n", "\n");
        assert_eq!(d, r, "{name}: debug vs release mismatch");
        assert_eq!(d, i, "{name}: debug vs interp mismatch");
    }
}



#[test]
fn mir_backend_matches_tree_backend_on_matches_and_counted_loops() {
    // The MIR backend is the default for every function the CFG lowers
    // completely (M2.3): matches with payload bindings and counted loops
    // included. The same program through both backends must agree byte for
    // byte — this pins the shapes the per-function fallback freed.
    let dir = unique_temp_dir("codegen_modes");
    let source = dir.join("flow.ae");
    fs::write(&source, r#"enum Op { Add(i64, i64), Neg(i64), Id(string), Halt }

fn eval(op: Op) -> i64 {
    match op {
        Op::Add(a, b) => { return a + b; }
        Op::Neg(a) => { return 0 - a; }
        Op::Id(name) => {
            if name == "seven" { return 7; }
            return 1;
        }
        Op::Halt => { return 0; }
    }
}

fn main() {
    var items = vec[1, 2, 3, 4, 5, 6];
    for x in items {
        if x == 3 { continue; }
        if x == 6 { break; }
        print(x);
    }
    var total = 0;
    for k in 0..4 {
        total = total + k;
    }
    print(total);
    print(eval(Op::Add(2, 3)));
    print(eval(Op::Neg(9)));
    print(eval(Op::Id("seven")));
    print(eval(Op::Halt));
    var seen = 0;
    for i in 0..3 {
        for j in 0..4 {
            if j == 2 { continue; }
            if i == 2 && j == 1 { break; }
            seen = seen + 1;
        }
    }
    print(seen);
    // A match over a payload the body mutates around: the subject stays
    // borrowed for the arm, and the join after the arms keeps flowing.
    var codes = vec[1, 2, 1];
    for c in codes {
        var op = Op::Neg(c);
        match op {
            Op::Neg(v) => { print(0 - v); }
            Op::Halt => { print(-999); }
            Op::Add(a, b) => { print(a + b); }
            Op::Id(text) => { print(len(text)); }
        }
    }
}
"#).unwrap();
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir = run("mir");
    let tree = run("tree");
    assert!(mir.status.success(), "mir failed: {}", String::from_utf8_lossy(&mir.stderr));
    assert!(tree.status.success(), "tree failed: {}", String::from_utf8_lossy(&tree.stderr));
    let mir_out = String::from_utf8_lossy(&mir.stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&tree.stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out, "backends disagree");
    // 1,2,4,5 (skip 3, break at 6); 6 (0+1+2+3); 5; -9; 7; 0; 7 (nested
    // continue lands on the inner step, break leaves the inner loop);
    // -1,-2,-1 (match inside a loop, one arm per variant).
    assert_eq!(mir_out, "1\n2\n4\n5\n6\n5\n-9\n7\n0\n7\n-1\n-2\n-1\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn user_defined_drop_runs_on_scope_exit_and_respects_move() {
    let dir = std::env::temp_dir().join("ae_test_user_drop");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
struct Guard {
    id: i64,
}

impl Guard {
    fn drop(&mut self) -> i64 {
        print(self.id);
        return 0;
    }
}

fn take(g: Guard) -> i64 {
    return 0;
}

fn early(flag: bool) -> i64 {
    var a = Guard { id: 10 };
    if flag {
        return 1;
    }
    var b = Guard { id: 20 };
    return 2;
}

fn maybe_value(flag: bool) -> Option[i64] {
    if flag { return Option[i64]::Some(42); }
    return Option[i64]::None();
}

fn propagate(flag: bool) -> Option[i64] {
    var guard = Guard { id: 30 };
    let value = maybe_value(flag)?;
    print(value);
    return Option[i64]::Some(value);
}

fn main() {
    if true {
        var g1 = Guard { id: 1 };
    }
    var g2 = Guard { id: 2 };
    var _r1 = take(g2);
    var _r2 = early(true);
    let _success = propagate(true);
    let _failure = propagate(false);
}
"#).unwrap();
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir = run("mir");
    assert!(mir.status.success(), "mir failed: {}", String::from_utf8_lossy(&mir.stderr));
    let mir_out = String::from_utf8_lossy(&mir.stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, "1\n2\n10\n42\n30\n30\n");
    let tree = run("tree");
    assert!(tree.status.success(), "tree failed: {}", String::from_utf8_lossy(&tree.stderr));
    assert_eq!(tree.stdout, mir.stdout);
    let graph = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-mir")
        .arg(&source)
        .output()
        .unwrap();
    assert!(graph.status.success(), "{}", String::from_utf8_lossy(&graph.stderr));
    let graph = String::from_utf8_lossy(&graph.stdout);
    assert!(graph.contains("switch v"), "{graph}");
    assert!(graph.contains("take_propagation_payload"), "{graph}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn user_defined_drop_rejects_direct_call_and_pub() {
    let dir = std::env::temp_dir().join("ae_test_user_drop_neg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let direct = dir.join("direct.ae");
    std::fs::write(&direct, r#"
struct G { id: i64 }
impl G {
    fn drop(&mut self) -> i64 { return 0; }
}
fn main() {
    var g = G { id: 1 };
    g.drop();
}
"#).unwrap();
    let out1 = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", direct.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out1.status.success(), "direct call to drop should be rejected");

    let pub_drop = dir.join("pub_drop.ae");
    std::fs::write(&pub_drop, r#"
struct G { id: i64 }
impl G {
    pub fn drop(&mut self) -> i64 { return 0; }
}
fn main() {}
"#).unwrap();
    let out2 = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", pub_drop.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out2.status.success(), "pub fn drop should be rejected");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ptr_drop_in_place_drops_struct_in_place_and_requires_unsafe() {
    let dir = std::env::temp_dir().join("ae_test_ptr_drop_in_place");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
struct Guard { id: i64 }
impl Guard {
    fn drop(&mut self) -> i64 {
        print(self.id);
        return 0;
    }
}

fn main() {
    var x = 77;
    unsafe {
        var p = &mut x as *mut i64;
        var _d = ptr_drop_in_place(p);
    }
    print(x);
}
"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", "mir")
        .output()
        .unwrap();
    assert!(out.status.success(), "mir failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "77\n");

    let safe_err = dir.join("safe_err.ae");
    std::fs::write(&safe_err, r#"
fn main() {
    var x = 1;
    var p = &mut x as *mut i64;
    var _d = ptr_drop_in_place(p);
}
"#).unwrap();
    let err = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check", safe_err.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!err.status.success(), "ptr_drop_in_place outside unsafe should be rejected");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn generic_struct_destructor_runs_on_scope_exit() {
    let dir = std::env::temp_dir().join("ae_test_generic_drop");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
struct Box[T] {
    tag: i64,
    val: T,
}

impl Box[T] {
    fn drop(&mut self) -> i64 {
        print(self.tag);
        return 0;
    }
}

fn main() {
    if true {
        var b1 = Box[i64] { tag: 11, val: 42 };
    }
    var b2 = Box[string] { tag: 22, val: "hi" };
}
"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", "mir")
        .output()
        .unwrap();
    assert!(out.status.success(), "mir failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "11\n22\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn self_hosted_vec_with_destructor_and_ptr_drop_in_place() {
    let dir = std::env::temp_dir().join("ae_test_self_hosted_vec");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
extern "C" fn ae_std_alloc(size: i64, line: i64, column: i64) -> *mut i64;
extern "C" fn ae_std_free(block: *mut i64) -> *mut i64;

#[repr(C)]
struct Elem {
    id: i64,
}

impl Elem {
    fn drop(&mut self) -> i64 {
        print(self.id);
        return 0;
    }
}

struct MyVec[T] {
    ptr: *mut i64,
    len: i64,
}

impl MyVec[T] {
    fn new() -> MyVec[T] {
        var elem_size = c_size_of[T]();
        unsafe {
            var raw = ae_std_alloc(elem_size * 2, 1, 1);
            return MyVec[T] { ptr: raw, len: 0 };
        }
    }

    fn append(&mut self, val: T) -> i64 {
        unsafe {
            var base = self.ptr as *mut T;
            var slot = base.offset(self.len);
            var _w = ptr_write(slot, val);
            self.len = self.len + 1;
        }
        return 0;
    }

    fn drop(&mut self) -> i64 {
        unsafe {
            var base = self.ptr as *mut T;
            var i = 0;
            while i < self.len {
                var elem_ptr = base.offset(i);
                var _d = ptr_drop_in_place(elem_ptr);
                i = i + 1;
            }
            var _f = ae_std_free(self.ptr);
        }
        return 0;
    }
}

fn main() {
    var v = MyVec[Elem]::new();
    var _p1 = v.append(Elem { id: 101 });
    var _p2 = v.append(Elem { id: 202 });
    print(v.len);
}
"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", "mir")
        .output()
        .unwrap();
    assert!(out.status.success(), "mir failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "2\n101\n202\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compound_assignment_operators() {
    let dir = std::env::temp_dir().join("ae_test_compound_assign");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
struct Counter { n: i64 }

fn main() {
    var x = 10;
    x += 5;
    x -= 3;
    x *= 2;
    x /= 4;
    x %= 4;
    print(x);

    var c = Counter { n: 10 };
    c.n += 5;
    c.n %= 6;
    print(c.n);

    var f = 1.5;
    f += 2.5;
    f *= 2.0;
    print(f);

    var s = "hello";
    s += " world";
    print(s);
}
"#).unwrap();
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir = run("mir");
    let tree = run("tree");
    assert!(mir.status.success(), "mir failed: {}", String::from_utf8_lossy(&mir.stderr));
    assert!(tree.status.success(), "tree failed: {}", String::from_utf8_lossy(&tree.stderr));
    let mir_out = String::from_utf8_lossy(&mir.stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&tree.stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out);
    assert_eq!(mir_out, "2\n3\n8.0\nhello world\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bitwise_operators_and_precedence() {
    let dir = std::env::temp_dir().join("ae_test_bitwise");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let source = dir.join("main.ae");
    std::fs::write(&source, r#"
fn main() {
    var a = 12;
    var b = 10;
    print(a & b);
    print(a | b);
    print(a ^ b);
    print(1 << 4);
    print(32 >> 2);
    print(1 | 2 ^ 3 & 6);
}
"#).unwrap();
    let run = |mode: &str| Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", source.to_str().unwrap()])
        .env("AE_CODEGEN", mode)
        .output()
        .unwrap();
    let mir = run("mir");
    let tree = run("tree");
    assert!(mir.status.success(), "mir failed: {}", String::from_utf8_lossy(&mir.stderr));
    assert!(tree.status.success(), "tree failed: {}", String::from_utf8_lossy(&tree.stderr));
    let mir_out = String::from_utf8_lossy(&mir.stdout).replace("\r\n", "\n");
    let tree_out = String::from_utf8_lossy(&tree.stdout).replace("\r\n", "\n");
    assert_eq!(mir_out, tree_out);
    // 12 & 10 = 8; 12 | 10 = 14; 12 ^ 10 = 6; 1 << 4 = 16; 32 >> 2 = 8;
    // 1 | (2 ^ (3 & 6)) = 1 | (2 ^ 2) = 1 | 0 = 1
    assert_eq!(mir_out, "8\n14\n6\n16\n8\n1\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mir_branch_observes_writes_through_raw_pointers() {
    let output = execute(r#"
fn main() {
    var flag = false;
    unsafe {
        var pointer = &mut flag as *mut bool;
        var _write = ptr_write(pointer, true);
    }
    if flag { print(42); } else { print(0); }
}
"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");
}

#[test]
fn mir_preserves_trapping_conditions_with_identical_successors() {
    let output = execute("fn main() { let zero = 0; if 1 / zero == 0 {} print(42); }");
    assert!(!output.status.success(), "the condition must still be evaluated");
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("division by zero"), "{error}");
}

#[test]
fn emit_mir_keeps_short_circuit_rhs_inside_the_lazy_operand() {
    let dir = unique_temp_dir("mir_short_circuit");
    let source = dir.join("short_circuit.ae");
    fs::write(&source, "fn show(x: i64) -> bool { print(x); return true; } fn identity(x: bool) -> bool { return x; } fn main() { let hit = identity(false && show(9)); print(hit); }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-mir").arg(&source).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mir = String::from_utf8_lossy(&output.stdout);
    assert!(mir.contains("false && lazy { fn#0(9) -> bool }"), "{mir}");
    assert!(!mir.contains("= fn#0(9)"), "{mir}");
}

#[test]
fn short_circuit_conditions_preserve_loop_and_branch_edges() {
    let dir = unique_temp_dir("mir_short_circuit_edges");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
fn show(x: i64) -> bool {
    print(x);
    return true;
}

fn main() {
    var i = 0;
    while i < 2 && show(i) {
        i = i + 1;
        continue;
    }
    if !(i == 2 || show(99)) { print(100); }
    print(i);
}
"#).unwrap();

    let cli = env!("CARGO_BIN_EXE_ae");
    let graph = Command::new(cli).arg("emit-mir").arg(&path).output().unwrap();
    let output = Command::new(cli).arg("run").arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(graph.status.success(), "{}", String::from_utf8_lossy(&graph.stderr));
    let graph = String::from_utf8_lossy(&graph.stdout);
    assert_eq!(graph.matches("branch fn#0(").count(), 2, "{graph}");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "0\n1\n2\n");
}

#[test]
fn short_circuit_returns_compute_the_value_before_scope_cleanup() {
    let output = execute(r#"
struct Guard { id: i64 }
impl Guard {
    fn drop(&mut self) -> i64 {
        print(self.id);
        return 0;
    }
}

fn mark(id: i64) -> bool {
    print(id);
    return true;
}

fn guarded(run: bool) -> bool {
    let guard = Guard { id: 42 };
    return run && mark(7);
}

fn main() {
    assert(true || mark(99));
    print(false && mark(100));
    print(guarded(false));
    print(guarded(true));
}
"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "false\n42\nfalse\n7\n42\ntrue\n");
}

#[test]
fn pointer_write_arguments_execute_left_to_right() {
    let output = execute(r#"
fn combine(left: i64, right: i64) -> i64 { return left + right; }
fn main() {
    var value = 0;
    unsafe {
        let pointer = &mut value as *mut i64;
        let _result = combine(ptr_write(pointer, 1), ptr_write(pointer, 2));
    }
    print(value);
}
"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\n");
}

#[test]
fn function_values_borrow_and_release_temporary_string_arguments() {
    let dir = unique_temp_dir("indirect_string_view");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
fn apply(f: fn(&string) -> i64) -> i64 {
    let suffix = "b";
    return f("a" + suffix);
}

fn main() {
    print(apply(|text: &string| -> i64 { return len(text); }));
}
"#).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let output = Command::new(cli).arg("run").arg(&path).output().unwrap();
    let c = Command::new(cli).arg("emit-c").arg(&path).output().unwrap();
    let mir = Command::new(cli).arg("emit-mir").arg(&path).output().unwrap();
    assert!(mir.status.success(), "{}", String::from_utf8_lossy(&mir.stderr));
    let mir = String::from_utf8_lossy(&mir.stdout);
    assert!(mir.lines().any(|line| line.contains("(string view) = string_view(")), "{mir}");
    fs::remove_dir_all(dir).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "2\n");
    assert!(c.status.success(), "{}", String::from_utf8_lossy(&c.stderr));
    let text = String::from_utf8_lossy(&c.stdout);
    let (_, call_and_tail) = text.split_once("= v0(AE_VIEW(").expect("indirect view call");
    let (temporary, tail) = call_and_tail.split_once("));").expect("end of indirect call");
    assert_eq!(tail.lines().find(|line| !line.trim().is_empty()).map(str::trim),
        Some(format!("ae_str_release(&{temporary});").as_str()));
}

#[test]
fn function_values_borrow_all_vector_shapes() {
    let dir = unique_temp_dir("indirect_vector_borrow");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
fn apply(f: fn(&vec, &vec[string], &vec[f64]) -> i64) -> i64 {
    let numbers = vec[40];
    let words = split("one two", " ");
    let floats = vec[0.5];
    let result = f(&numbers, &words, &floats);
    print(numbers[0]);
    print(words[1]);
    print(floats[0] > 0.0);
    return result;
}

fn main() {
    print(apply(|numbers: &vec, words: &vec[string], floats: &vec[f64]| -> i64 {
        return numbers[0] + len(words) + len(floats);
    }));
}
"#).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).arg("run").arg(&path).args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "40\ntwo\ntrue\n43\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn function_values_borrow_structs_and_fixed_arrays() {
    let dir = unique_temp_dir("indirect_struct_array_borrow");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
struct Pair { left: i64, right: i64 }

fn apply(f: fn(&Pair, &[i64; 2]) -> i64, pair: &Pair, values: &[i64; 2]) -> i64 {
    return f(&pair, &values);
}

fn main() {
    var pair = Pair { left: 40, right: 2 };
    var values = [1, 2];
    print(apply(|p: &Pair, a: &[i64; 2]| -> i64 {
        return p.left + a[1];
    }, &pair, &values));
    pair.right = 3;
    values[0] = 4;
    print(pair.right + values[0]);
}
"#).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let graph = Command::new(cli).arg("emit-mir").arg(&path).output().unwrap();
    assert!(graph.status.success(), "{}", String::from_utf8_lossy(&graph.stderr));
    let graph = String::from_utf8_lossy(&graph.stdout);
    assert!(graph.contains("forward_borrow(v1)") && graph.contains("address_of(v0)"), "{graph}");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).arg("run").arg(&path).args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n7\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn function_values_forward_mutable_struct_borrows_and_check_live_views() {
    let dir = unique_temp_dir("indirect_mut_borrow");
    let path = dir.join("main.ae");
    let declarations = r#"
struct Entry { text: string }

fn apply(f: fn(&mut Entry) -> i64, entry: &mut Entry) -> i64 {
    return f(&mut entry);
}
"#;
    fs::write(&path, format!("{declarations}{}", r#"
fn main() {
    var entry = Entry { text: "before" };
    let _result = apply(|item: &mut Entry| -> i64 {
        item.text = "after";
        return 0;
    }, &mut entry);
    print(entry.text);
}
"#)).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).arg("run").arg(&path).args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "after\n");
    }

    fs::write(&path, r#"
struct Entry { text: string }

fn bad(f: fn(&mut Entry) -> i64) -> i64 {
    var entry = Entry { text: "before" };
    let view = slice_view(entry.text, 0, 1);
    let result = f(&mut entry);
    print(view);
    return result;
}

fn main() {}
"#).unwrap();
    let checked = Command::new(cli).arg("check").arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(!checked.status.success());
    let stderr = String::from_utf8_lossy(&checked.stderr);
    assert!(stderr.contains("while a view binding of it may still be read"), "{stderr}");
}

#[test]
fn forwarded_mutable_borrows_cannot_alias_call_arguments() {
    let dir = unique_temp_dir("forwarded_mut_alias");
    let path = dir.join("main.ae");
    for (second_type, second_argument) in [
        ("&mut Pair", "&mut pair"),
        ("&Pair", "&pair"),
    ] {
        for (signature, arguments) in [
            (format!("&mut Pair, {second_type}"), format!("&mut pair, {second_argument}")),
            (format!("{second_type}, &mut Pair"), format!("{second_argument}, &mut pair")),
        ] {
            fs::write(&path, format!(r#"
struct Pair {{ value: i64 }}
fn forward(f: fn({signature}) -> i64, pair: &mut Pair) -> i64 {{
    return f({arguments});
}}
fn main() {{}}
"#)).unwrap();
            let checked = Command::new(env!("CARGO_BIN_EXE_ae"))
                .arg("check").arg(&path).output().unwrap();
            let stderr = String::from_utf8_lossy(&checked.stderr);
            assert!(!checked.status.success() && stderr.contains("borrowed in this statement"), "{stderr}");
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn forwarded_method_receivers_cannot_alias_borrowed_arguments() {
    let dir = unique_temp_dir("forwarded_receiver_alias");
    let path = dir.join("main.ae");
    for (receiver, other_type, argument) in [
        ("&mut self", "&mut Pair", "&mut pair"),
        ("&mut self", "&Pair", "&pair"),
        ("&self", "&mut Pair", "&mut pair"),
    ] {
        fs::write(&path, format!(r#"
struct Pair {{ value: i64 }}
impl Pair {{
    fn combine({receiver}, other: {other_type}) -> i64 {{
        return self.value + other.value;
    }}
}}
fn forward(pair: &mut Pair) -> i64 {{
    return pair.combine({argument});
}}
fn main() {{}}
"#)).unwrap();
        let checked = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
        let stderr = String::from_utf8_lossy(&checked.stderr);
        assert!(!checked.status.success() && stderr.contains("borrowed in this statement"), "{stderr}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn local_noncapturing_closures_and_function_parameters_can_be_forwarded() {
    let dir = unique_temp_dir("function_forwarding");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }
fn forward(f: fn(i64) -> i64, x: i64) -> i64 { return apply(f, x); }
fn main() {
    let increment = |x: i64| -> i64 { return x + 1; };
    print(forward(increment, 41));
}
"#).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).arg("run").arg(&path).args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn named_functions_can_be_passed_with_a_matching_safe_signature() {
    let dir = unique_temp_dir("named_function_arguments");
    let path = dir.join("main.ae");
    let apply = "fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }";
    fs::write(&path, format!(r#"
{apply}
fn main() {{ print(apply(double, 21)); print(apply(callback, 40)); }}
fn double(x: i64) -> i64 {{ return x + x; }}
#[no_mangle]
extern "C" fn callback(x: i64) -> i64 {{ return x + 2; }}
"#)).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let graph = Command::new(cli).arg("emit-mir").arg(&path).output().unwrap();
    assert!(graph.status.success(), "{}", String::from_utf8_lossy(&graph.stderr));
    let graph = String::from_utf8_lossy(&graph.stdout);
    assert!(graph.contains("fnptr#") && graph.contains("fn#0("), "{graph}");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).arg("run").arg(&path).args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n42\n");
    }
    for (definition, expected) in [
        ("fn candidate(x: bool) -> i64 { return 0; }", "different signature"),
        ("extern \"C\" fn candidate(x: i64) -> i64;", "cannot be passed as a safe function value"),
        ("/// Safety: this fixture tests callback visibility.\nunsafe fn candidate(x: i64) -> i64 { return x; }",
         "cannot be passed as a safe function value"),
    ] {
        fs::write(&path, format!("{apply}\nfn main() {{ print(apply(candidate, 21)); }}\n{definition}\n")).unwrap();
        let checked = Command::new(cli).arg("check").arg(&path).output().unwrap();
        let stderr = String::from_utf8_lossy(&checked.stderr);
        assert!(!checked.status.success() && stderr.contains(expected), "{stderr}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn public_functions_reject_private_types_in_callback_signatures() {
    let dir = unique_temp_dir("private_callback_types");
    let path = dir.join("main.ae");
    let cli = env!("CARGO_BIN_EXE_ae");
    for signature in ["fn(&Hidden) -> i64", "fn(&mut Hidden) -> i64", "fn() -> Hidden", "&mut Hidden"] {
        fs::write(&path, format!(r#"
struct Hidden {{ value: i64 }}
pub fn expose(callback: {signature}) -> i64 {{ return 0; }}
fn main() {{}}
"#)).unwrap();
        let checked = Command::new(cli).arg("check").arg(&path).output().unwrap();
        let stderr = String::from_utf8_lossy(&checked.stderr);
        assert!(!checked.status.success() && stderr.contains("public function 'expose' exposes private struct 'Hidden'"), "{stderr}");
    }
    fs::write(&path, r#"
pub struct Visible { value: i64 }
pub fn expose(callback: fn(&Visible) -> i64) -> i64 { return 0; }
fn main() {}
"#).unwrap();
    let checked = Command::new(cli).arg("check").arg(&path).output().unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stderr));
}

#[test]
fn native_backends_evaluate_scope_exit_operands_before_running_destructors() {
    let dir = unique_temp_dir("return_before_drop");
    let path = dir.join("main.ae");
    fs::write(&path, r#"
struct Guard { target: *mut i64 }
impl Guard {
    fn drop(&mut self) -> i64 {
        unsafe { let _write = ptr_write(self.target, 0); }
        return 0;
    }
}

fn answer() -> i64 {
    var result = 42;
    let guard = Guard { target: &mut result as *mut i64 };
    return result;
}

fn main() {
    print(answer());
    var status = 20;
    let guard = Guard { target: &mut status as *mut i64 };
    exit(status + 2);
}
"#).unwrap();
    for backend in ["mir", "tree"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("run").arg(&path).env("AE_CODEGEN", backend).output().unwrap();
        assert_eq!(output.status.code(), Some(22), "{backend}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn package_manifest_selects_entry_and_validates_metadata() {
    let dir = unique_temp_dir("package_manifest");
    fs::create_dir(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.ae"), "fn main() { print(42); }").unwrap();
    let manifest = dir.join("ae.toml");
    let valid = "[package]\nname = 'sample-tool'\nversion = '0.1.0'\nae = '0.1'\n";
    fs::write(&manifest, valid).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let output = Command::new(cli).arg("run").arg(&dir).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");

    let nested = dir.join("src/nested");
    fs::create_dir(&nested).unwrap();
    let implicit = Command::new(cli).current_dir(&nested).args(["run", "--interp"]).output().unwrap();
    assert!(implicit.status.success(), "{}", String::from_utf8_lossy(&implicit.stderr));
    assert_eq!(String::from_utf8_lossy(&implicit.stdout).replace("\r\n", "\n"), "42\n");
    let implicit = Command::new(cli).current_dir(&nested).args(["check", "--json"]).output().unwrap();
    assert!(implicit.status.success(), "{}", String::from_utf8_lossy(&implicit.stdout));
    assert_eq!(String::from_utf8_lossy(&implicit.stdout).trim(), "[]");
    let checked = Command::new(cli).args(["check", "--json"]).arg(&manifest).output().unwrap();
    assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stdout));
    assert_eq!(String::from_utf8_lossy(&checked.stdout).trim(), "[]");
    for (source, message) in [
        (valid.replace("sample-tool", "UpperCase"), "package.name"),
        (valid.replace("0.1.0", "0.1"), "package.version"),
        (valid.replace("'0.1'", "'99.0'"), "requires ae 99.0"),
        (format!("{valid}\n[dependencies]\nutil = '1.0'\n"), "dependency resolution"),
        ("[package\n".to_owned(), "TOML"),
    ] {
        fs::write(&manifest, source).unwrap();
        let checked = Command::new(cli).arg("check").arg(&manifest).output().unwrap();
        let stderr = String::from_utf8_lossy(&checked.stderr);
        assert!(!checked.status.success() && stderr.contains(message), "{stderr}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn new_scaffolds_a_runnable_package_without_overwriting() {
    let dir = unique_temp_dir("new_package");
    let cli = env!("CARGO_BIN_EXE_ae");
    let project = dir.join("sample-tool");
    let created = Command::new(cli).arg("new").arg(&project).output().unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let manifest = fs::read(project.join("ae.toml")).unwrap();
    let output = Command::new(cli).current_dir(&project).arg("run").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "Hello, ae!\n");

    let source = project.join("src/main.ae");
    fs::write(&source, "fn main() {}").unwrap();
    let retry = Command::new(cli).arg("new").arg(&project).output().unwrap();
    assert!(!retry.status.success());
    assert_eq!(fs::read_to_string(source).unwrap(), "fn main() {}");
    assert_eq!(fs::read(project.join("ae.toml")).unwrap(), manifest);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn init_scaffolds_into_an_existing_directory_without_overwriting_sources() {
    let dir = unique_temp_dir("init_package");
    let cli = env!("CARGO_BIN_EXE_ae");
    let init = |cwd: &std::path::Path, target: Option<&str>| {
        let mut command = Command::new(cli);
        command.current_dir(cwd).arg("init");
        if let Some(target) = target { command.arg(target); }
        command.output().unwrap()
    };
    let plain = dir.join("plain-tool");
    fs::create_dir(&plain).unwrap();
    let created = init(&plain, None);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let output = Command::new(cli).current_dir(&plain).arg("run").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "Hello, ae!\n");

    let manifest = fs::read(plain.join("ae.toml")).unwrap();
    let again = init(&plain, None);
    let stderr = String::from_utf8_lossy(&again.stderr);
    assert!(!again.status.success() && stderr.contains("already contains ae.toml"), "{stderr}");
    assert_eq!(fs::read(plain.join("ae.toml")).unwrap(), manifest);

    let existing = dir.join("existing-tool");
    fs::create_dir_all(existing.join("src")).unwrap();
    fs::write(existing.join("src/main.ae"), "fn main() { print(7); }").unwrap();
    let adopted = init(&dir, Some("existing-tool"));
    assert!(adopted.status.success(), "{}", String::from_utf8_lossy(&adopted.stderr));
    let output = Command::new(cli).current_dir(&existing).arg("run").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n");

    let missing = init(&dir, Some("absent-tool"));
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(!missing.status.success() && stderr.contains("init needs an existing directory"), "{stderr}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn package_deps_visits_local_diamond_once_and_rejects_cycles() {
    let dir = unique_temp_dir("package_dependency_graph");
    let write_manifest = |name: &str, dependencies: &str| {
        let package = dir.join(name);
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("ae.toml"), format!(
            "[package]\nname = '{name}'\nversion = '0.1.0'\nae = '0.1'\n\n[dependencies]\n{dependencies}"
        )).unwrap();
    };
    write_manifest("app", "left = { path = '../left' }\nright = { path = '../right' }");
    write_manifest("left", "shared = { path = '../shared' }");
    write_manifest("right", "shared = { path = '../shared' }");
    write_manifest("shared", "");
    let cli = env!("CARGO_BIN_EXE_ae");
    let run = || Command::new(cli).current_dir(dir.join("app")).arg("deps").output().unwrap();
    let output = run();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let order: Vec<_> = stdout.lines().filter(|line| !line.starts_with(' '))
        .map(|line| line.split_whitespace().next().unwrap()).collect();
    assert_eq!(order, ["shared", "left", "right", "app"]);

    write_manifest("shared", "app = { path = '../app' }");
    let cycle = run();
    let error = String::from_utf8_lossy(&cycle.stderr);
    assert!(!cycle.status.success() && error.contains("dependency cycle") && error.matches("ae.toml").count() == 4, "{error}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn path_dependency_versions_must_satisfy_caret_requirements() {
    let dir = unique_temp_dir("package_version_constraints");
    let write = |name: &str, version: &str, dependencies: &str| {
        let package = dir.join(name);
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("ae.toml"), format!(
            "[package]\nname = '{name}'\nversion = '{version}'\nae = '0.1'\n\n[dependencies]\n{dependencies}"
        )).unwrap();
    };
    write("app", "0.1.0", "lib = { path = '../lib', version = '0.3' }");
    let cli = env!("CARGO_BIN_EXE_ae");
    let deps = || Command::new(cli).current_dir(dir.join("app")).arg("deps").output().unwrap();
    for (version, satisfied) in [("0.3.0", true), ("0.3.9", true), ("0.2.9", false), ("0.4.0", false)] {
        write("lib", version, "");
        let output = deps();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if satisfied {
            assert!(output.status.success(), "{version}: {stderr}");
            assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("lib {version}")), "{version}");
        } else {
            assert!(!output.status.success(), "{version}");
            assert!(stderr.contains("requires version 0.3 but") && stderr.contains(version), "{version}: {stderr}");
        }
    }
    write("lib", "0.3.0", "");
    for (declaration, message) in [
        ("lib = { path = '../lib', version = '0.3.x' }", "must be a major.minor version"),
        ("lib = { path = '../lib', version = 3 }", "version must be a string"),
        ("lib = { path = '../lib', registry = 'x' }", "unsupported key 'registry'"),
    ] {
        write("app", "0.1.0", declaration);
        let output = deps();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success() && stderr.contains(message), "{declaration}: {stderr}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn local_library_packages_compile_through_declared_aliases() {
    let dir = unique_temp_dir("package_library_imports");
    for (name, file, source, dependency) in [
        ("app", "main.ae", "use \"@math\"; fn main() { print(answer()); }", "math = { path = '../math' }"),
        ("math", "lib.ae", "use \"@base\"; pub fn answer() -> i64 { return base() + 2; }", "base = { path = '../base' }"),
        ("base", "lib.ae", "pub fn base() -> i64 { return 40; }", ""),
    ] {
        let package = dir.join(name);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(package.join("ae.toml"), format!(
            "[package]\nname = '{name}'\nversion = '0.1.0'\nae = '0.1'\n\n[dependencies]\n{dependency}"
        )).unwrap();
        fs::write(package.join("src").join(file), source).unwrap();
    }
    let cli = env!("CARGO_BIN_EXE_ae");
    for flags in [&[][..], &["--interp"][..]] {
        let output = Command::new(cli).current_dir(dir.join("app")).arg("run").args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");
    }
    let app = dir.join("app");
    fs::write(app.join("src/test.ae"), r#"
use "@math";
fn test_answer() -> i64 {
    assert(answer() == 42);
    return 0;
}
"#).unwrap();
    for flags in [&[][..], &["--interp"][..]] {
        let tests = Command::new(cli).current_dir(app.join("src")).arg("test").args(flags).output().unwrap();
        assert!(tests.status.success(), "{}", String::from_utf8_lossy(&tests.stderr));
        assert!(String::from_utf8_lossy(&tests.stdout).contains("test_answer"));
    }
    let cache = dir.join("cache");
    let native_run = || Command::new(cli).current_dir(&app)
        .env("AE_CACHE", &cache).env("AE_TIMINGS", "1").arg("run").output().unwrap();
    assert!(native_run().status.success());
    let cached = native_run();
    assert!(cached.status.success());
    assert!(String::from_utf8_lossy(&cached.stderr).contains("frontend-cache: hit"));
    let executable = app.join("tool.exe");
    let build = || Command::new(cli).current_dir(&app).env("AE_CACHE", &cache).args(["build", "-o"]).arg(&executable).output().unwrap();
    let missing = build();
    assert!(!missing.status.success() && String::from_utf8_lossy(&missing.stderr).contains("Run ae update"));
    let update = || Command::new(cli).current_dir(&app).arg("update").output().unwrap();
    let updated = update();
    assert!(updated.status.success(), "{}", String::from_utf8_lossy(&updated.stderr));
    let lockfile = app.join("ae.lock");
    let lock = fs::read(&lockfile).unwrap();
    assert!(update().status.success());
    assert_eq!(fs::read(lockfile).unwrap(), lock);

    let dependency = dir.join("base/src/lib.ae");
    fs::write(&dependency, "pub fn base() -> i64 { return 41; }").unwrap();
    assert!(native_run().status.success());
    let stale = build();
    let stderr = String::from_utf8_lossy(&stale.stderr);
    assert!(!stale.status.success() && stderr.contains("is stale") && stderr.contains("../base/src/lib.ae"), "{stderr}");
    assert!(!executable.exists());
    assert!(update().status.success());
    let compiled = build();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let result = Command::new(&executable).output().unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8_lossy(&result.stdout).replace("\r\n", "\n"), "43\n");
    let alternate = dir.join("alternate");
    fs::create_dir_all(alternate.join("src")).unwrap();
    fs::write(alternate.join("ae.toml"), "[package]\nname = 'alternate'\nversion = '0.1.0'\nae = '0.1'\n").unwrap();
    fs::write(alternate.join("src/lib.ae"), "pub fn base() -> i64 { return 100; }").unwrap();
    let manifest = dir.join("math/ae.toml");
    let contents = fs::read_to_string(&manifest).unwrap().replace("'../base'", "'../alternate'");
    fs::write(manifest, contents).unwrap();
    let remapped = native_run();
    assert!(remapped.status.success(), "{}", String::from_utf8_lossy(&remapped.stderr));
    assert!(String::from_utf8_lossy(&remapped.stderr).contains("frontend-cache: miss"));
    assert_eq!(String::from_utf8_lossy(&remapped.stdout).replace("\r\n", "\n"), "102\n");
    fs::write(dir.join("app/src/main.ae"), "use \"@base\"; fn main() {}").unwrap();
    let checked = Command::new(cli).current_dir(dir.join("app")).args(["check", "--json"]).output().unwrap();
    let report = String::from_utf8_lossy(&checked.stdout);
    assert!(!checked.status.success() && report.contains("unknown package dependency 'base'"), "{report}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn add_and_remove_manage_manifest_dependencies_and_the_lockfile() {
    let dir = unique_temp_dir("package_add_remove");
    let app = dir.join("app");
    for (package, source) in [
        ("app", "use \"@math\";\nfn main() { print(answer()); }\n"),
        ("math", "pub fn answer() -> i64 { return 42; }\n"),
    ] {
        let root = dir.join(package);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("ae.toml"), format!(
            "[package]\nname = '{package}'\nversion = '0.1.0'\nae = '0.1'\n"
        )).unwrap();
        let file = if package == "app" { "main.ae" } else { "lib.ae" };
        fs::write(root.join("src").join(file), source).unwrap();
    }
    let cli = env!("CARGO_BIN_EXE_ae");
    let in_app = |args: &[&str]| Command::new(cli).current_dir(&app).args(args).output().unwrap();

    // Adding an undeclared dependency edits the manifest and refreshes the
    // lock, so an ordinary build works without a separate update step.
    let added = in_app(&["add", "../math"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(String::from_utf8_lossy(&added.stdout).contains("added dependency 'math' at ../math"));
    let manifest = fs::read_to_string(app.join("ae.toml")).unwrap();
    assert!(manifest.contains("[dependencies]\nmath = { path = \"../math\" }"), "{manifest}");
    assert!(app.join("ae.lock").exists());
    let run = in_app(&["run"]);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");

    // Re-adding the same source is idempotent. The recorded `../math` must
    // resolve to the same directory on every platform (Windows canonicalizes
    // to verbatim paths that do not fold `..`).
    let again = in_app(&["add", "../math"]);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert!(String::from_utf8_lossy(&again.stdout).contains("is already declared"));
    assert_eq!(fs::read_to_string(app.join("ae.toml")).unwrap(), manifest);

    // The same package name from another directory is a different source.
    let other = dir.join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    fs::write(other.join("ae.toml"), "[package]\nname = 'math'\nversion = '0.1.0'\nae = '0.1'\n").unwrap();
    fs::write(other.join("src/lib.ae"), "pub fn answer() -> i64 { return 0; }\n").unwrap();
    let conflict = in_app(&["add", "../other"]);
    let conflict_error = String::from_utf8_lossy(&conflict.stderr);
    assert!(!conflict.status.success()
        && conflict_error.contains("already declared with a different source"), "{conflict_error}");

    // Rejections happen before the manifest is touched.
    let before = fs::read_to_string(app.join("ae.toml")).unwrap();
    let self_add = in_app(&["add", "."]);
    assert!(!self_add.status.success()
        && String::from_utf8_lossy(&self_add.stderr).contains("a package cannot depend on itself"));
    let missing = in_app(&["remove", "nope"]);
    assert!(!missing.status.success()
        && String::from_utf8_lossy(&missing.stderr).contains("no dependency named 'nope'"));
    assert_eq!(fs::read_to_string(app.join("ae.toml")).unwrap(), before);

    // Removing a dependency whose alias is still imported succeeds (the
    // manifest edit is the requested change) and reports the stale lock; the
    // next run fails closed on the unknown import.
    let removed = in_app(&["remove", "math"]);
    assert!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr));
    let removed_output = String::from_utf8_lossy(&removed.stdout);
    assert!(removed_output.contains("removed dependency 'math'") && removed_output.contains("stale"), "{removed_output}");
    let manifest = fs::read_to_string(app.join("ae.toml")).unwrap();
    assert!(!manifest.contains("math"), "{manifest}");
    let broken = in_app(&["run"]);
    assert!(!broken.status.success()
        && String::from_utf8_lossy(&broken.stderr).contains("unknown package dependency 'math'"));

    // Once the import is gone too, `ae update` rebuilds the lock without
    // the removed dependency and the package runs again.
    fs::write(app.join("src/main.ae"), "fn main() { print(7); }\n").unwrap();
    let update = in_app(&["update"]);
    assert!(update.status.success(), "{}", String::from_utf8_lossy(&update.stderr));
    let lock = fs::read_to_string(app.join("ae.lock")).unwrap();
    assert!(!lock.contains("../math"), "{lock}");
    let run = in_app(&["run"]);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "7\n");
    let removed_twice = in_app(&["remove", "math"]);
    assert!(!removed_twice.status.success()
        && String::from_utf8_lossy(&removed_twice.stderr).contains("no dependency named 'math'"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn package_target_tables_load_only_the_active_local_dependency() {
    let dir = unique_temp_dir("package_targets");
    let app = dir.join("app");
    let lib = dir.join("lib");
    for package in [&app, &lib] {
        fs::create_dir_all(package.join("src")).unwrap();
    }
    fs::write(lib.join("ae.toml"), "[package]\nname = 'lib'\nversion = '0.1.0'\nae = '0.1'\n").unwrap();
    fs::write(lib.join("src/lib.ae"), "pub fn answer() -> i64 { return 42; }").unwrap();
    fs::write(app.join("src/main.ae"), "use \"@platform\"; fn main() { print(answer()); }").unwrap();
    let (active, inactive) = if cfg!(windows) { ("windows", "unix") } else { ("unix", "windows") };
    let manifest = app.join("ae.toml");
    let valid = format!(r#"
[package]
name = "app"
version = "0.1.0"
ae = "0.1"

[target.'cfg({active})'.dependencies]
platform = {{ path = "../lib" }}

[target.'cfg({inactive})'.dependencies]
platform = {{ path = "../absent" }}
"#);
    fs::write(&manifest, &valid).unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let update = Command::new(cli).current_dir(&app).arg("update").output().unwrap();
    assert!(update.status.success(), "{}", String::from_utf8_lossy(&update.stderr));
    let lock = fs::read_to_string(app.join("ae.lock")).unwrap();
    assert!(lock.contains("../lib/src/lib.ae") && !lock.contains("../absent"), "{lock}");
    let output = Command::new(cli).current_dir(&app).arg("run").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");

    fs::write(&manifest, format!("{valid}\n[dependencies]\nplatform = {{ path = '../other' }}\n")).unwrap();
    let invalid = Command::new(cli).current_dir(&app).arg("check").output().unwrap();
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(!invalid.status.success() && stderr.contains("multiple active declarations"), "{stderr}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lockfile_is_independent_of_dependency_order_and_working_directory() {
    let dir = unique_temp_dir("package_lock_determinism");
    for (name, body) in [("left", "pub fn left() -> i64 { return 1; }"), ("right", "pub fn right() -> i64 { return 2; }")] {
        let package = dir.join(name);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(package.join("ae.toml"), format!(
            "[package]\nname = '{name}'\nversion = '0.2.0'\nae = '0.1'\n"
        )).unwrap();
        fs::write(package.join("src/lib.ae"), body).unwrap();
    }
    let app = dir.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(app.join("src/main.ae"), "fn main() {}").unwrap();
    let manifest = app.join("ae.toml");
    let header = "[package]\nname = 'app'\nversion = '0.1.0'\nae = '0.1'\n\n[dependencies]\n";
    fs::write(&manifest, format!("{header}left = {{ path = '../left' }}\nright = {{ path = '../right' }}")).unwrap();

    let cli = env!("CARGO_BIN_EXE_ae");
    let update = |directory: &std::path::Path| {
        let output = Command::new(cli).current_dir(directory).arg("update").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    let lockfile = app.join("ae.lock");
    // The root manifest is content-addressed, so reordering its entries changes its own digest;
    // the resolved graph must not move.
    let graph = |text: &str| -> Vec<String> {
        text.lines().map(|line| {
            let mut fields = line.splitn(3, ' ');
            let kind = fields.next().unwrap_or("").to_owned();
            let path = fields.nth(1).unwrap_or("");
            format!("{kind} {path}")
        }).collect()
    };
    let digest_of = |text: &str, path: &str| -> String {
        text.lines().filter(|line| line.starts_with("manifest ") && line.contains(path))
            .map(|line| line.split(' ').nth(1).unwrap().to_owned()).next().unwrap()
    };
    update(&app);
    let first = fs::read_to_string(&lockfile).unwrap();
    assert_eq!(graph(&first).iter().filter(|line| line.starts_with("manifest ")).count(), 3, "{first}");
    assert!(first.lines().last().is_some_and(|line| line.starts_with("source ")));

    fs::write(&manifest, format!("{header}right = {{ path = '../right' }}\n\n# trailing comment\nleft = {{ path = '../left' }}")).unwrap();
    update(&app);
    let reordered = fs::read_to_string(&lockfile).unwrap();
    assert_eq!(graph(&reordered), graph(&first), "dependency order changed the locked graph");
    assert_eq!(digest_of(&reordered, "../left/ae.toml"), digest_of(&first, "../left/ae.toml"));
    assert_eq!(digest_of(&reordered, "../right/ae.toml"), digest_of(&first, "../right/ae.toml"));

    let nested = app.join("src/deep/more");
    fs::create_dir_all(&nested).unwrap();
    update(&nested);
    assert_eq!(fs::read_to_string(&lockfile).unwrap(), reordered, "working directory changed the lockfile");

    let build = Command::new(cli).current_dir(&nested).args(["build", "-o"]).arg(dir.join("app.bin")).output().unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn cli_exit_codes_distinguish_usage_compile_and_program_failures() {
    let dir = unique_temp_dir("exit_codes");
    let cli = env!("CARGO_BIN_EXE_ae");
    let usage = Command::new(cli).output().unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    assert!(String::from_utf8_lossy(&usage.stderr).starts_with("ae: usage: ae"), "{}", String::from_utf8_lossy(&usage.stderr));

    let unknown = Command::new(cli).arg("bogus").output().unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown command: bogus"));

    let missing = Command::new(cli).args(["check", "absent-file.ae"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(1), "an unreadable input is a toolchain failure, like rustc");
    assert!(missing.stdout.is_empty() && !missing.stderr.is_empty());

    let broken = dir.join("broken.ae");
    fs::write(&broken, "fn main() { let x: i64 = true; }").unwrap();
    let rejected = Command::new(cli).arg("check").arg(&broken).output().unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty() && !rejected.stderr.is_empty());

    let exiting = dir.join("exiting.ae");
    fs::write(&exiting, "fn main() { print(1); exit(7); }").unwrap();
    for flags in [&[][..], &["--interp"][..]] {
        let run = Command::new(cli).arg("run").arg(&exiting).args(flags).output().unwrap();
        assert_eq!(run.status.code(), Some(7), "{flags:?}: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "1\n");
    }
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn one_program_cannot_link_two_versions_of_the_same_package() {
    let dir = unique_temp_dir("package_version_conflict");
    let write = |directory: &str, name: &str, version: &str, dependencies: &str| {
        let package = dir.join(directory);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(package.join("src/lib.ae"), "pub fn value() -> i64 { return 1; }").unwrap();
        fs::write(package.join("ae.toml"), format!(
            "[package]\nname = '{name}'\nversion = '{version}'\nae = '0.1'\n\n[dependencies]\n{dependencies}"
        )).unwrap();
    };
    write("app", "app", "0.1.0", "left = { path = '../left' }\nright = { path = '../right' }");
    write("left", "left", "0.1.0", "shared = { path = '../shared-1' }");
    write("right", "right", "0.1.0", "shared = { path = '../shared-2' }");
    write("shared-1", "shared", "0.1.0", "");
    write("shared-2", "shared", "0.2.0", "");

    let cli = env!("CARGO_BIN_EXE_ae");
    let deps = || Command::new(cli).current_dir(dir.join("app")).arg("deps").output().unwrap();
    let conflict = deps();
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(!conflict.status.success(), "{stderr}");
    assert!(stderr.contains("package 'shared' is version 0.2.0 here but version 0.1.0"), "{stderr}");
    assert!(stderr.contains("shared-2") && stderr.contains("shared-1"), "{stderr}");

    write("shared-2", "shared", "0.1.0", "");
    let agreed = deps();
    assert!(agreed.status.success(), "{}", String::from_utf8_lossy(&agreed.stderr));
    assert_eq!(String::from_utf8_lossy(&agreed.stdout).matches("shared 0.1.0").count(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malformed_dependency_declarations_are_reported_at_the_manifest_location() {
    let dir = unique_temp_dir("package_malformed_manifest");
    let app = dir.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(app.join("src/main.ae"), "fn main() {}\n").unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let manifest_path = app.join("ae.toml");
    let check = |dependency: &str| {
        fs::write(&manifest_path, format!(
            "[package]\nname = 'app'\nversion = '0.1.0'\nae = '0.1'\n\n[dependencies]\n{dependency}"
        )).unwrap();
        let output = Command::new(cli).current_dir(&app).arg("deps").output().unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stderr).to_string())
    };
    for (dependency, needle) in [
        ("lib = 'not a table'", "requires a nonempty path table"),
        ("lib = { path = '' }", "requires a nonempty path table"),
        ("lib = { path = '/abs' }", "path must be relative to its manifest"),
        ("lib = { path = '../lib', version = 3 }", "version must be a string"),
        ("lib = { path = '../lib', version = '3' }", "must be a major.minor version"),
        ("lib = { path = '../lib', registry = 'x' }", "unsupported key 'registry'"),
    ] {
        let (ok, stderr) = check(dependency);
        assert!(!ok, "{dependency}: {stderr}");
        // The location is the declaring manifest, not just a bare message.
        assert!(stderr.contains(manifest_path.file_name().unwrap().to_str().unwrap())
            && stderr.contains(manifest_path.parent().unwrap().file_name().unwrap().to_str().unwrap()),
            "{dependency}: {stderr}");
        assert!(stderr.contains(needle), "{dependency}: {stderr}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn doc_command_renders_comments_and_runs_fenced_examples() {
    let dir = unique_temp_dir("doc_command");
    let file = dir.join("doc.ae");
    fs::write(&file, "/// Doubles a value.\n///\n/// ```ae\n/// fn main() { print(2); }\n/// ```\npub fn double(x: i64) -> i64 { return x * 2; }\n\nfn main() { print(double(1)); }\n").unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let rendered = Command::new(cli).arg("doc").arg(&file).output().unwrap();
    assert!(rendered.status.success(), "{}", String::from_utf8_lossy(&rendered.stderr));
    let text = String::from_utf8_lossy(&rendered.stdout);
    assert!(text.contains("# doc.ae"), "{text}");
    assert!(text.contains("## `pub fn double(x: i64) -> i64`"), "{text}");
    assert!(text.contains("Doubles a value."), "{text}");

    let tested = Command::new(cli).args(["doc", "--test"]).arg(&file).output().unwrap();
    assert!(tested.status.success(), "{}", String::from_utf8_lossy(&tested.stderr));
    assert!(String::from_utf8_lossy(&tested.stdout).contains("ok: doc example"), "{}", String::from_utf8_lossy(&tested.stdout));

    let broken = dir.join("broken.ae");
    fs::write(&broken, "/// ```ae\n/// fn main() { let x: i64 = true; }\n/// ```\nfn main() {}\n").unwrap();
    let failed = Command::new(cli).args(["doc", "--test"]).arg(&broken).output().unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert_eq!(failed.status.code(), Some(1));
    assert!(stderr.contains("broken.ae:1") && stderr.contains("doc example failed"), "{stderr}");

    let unclosed = dir.join("unclosed.ae");
    fs::write(&unclosed, "/// ```ae\n/// fn main() {}\nfn main() {}\n").unwrap();
    let leak = Command::new(cli).args(["doc", "--test"]).arg(&unclosed).output().unwrap();
    let stderr = String::from_utf8_lossy(&leak.stderr);
    assert_eq!(leak.status.code(), Some(1));
    assert!(stderr.contains("unclosed.ae: line 1: doc-comment code fence is never closed"), "{stderr}");

    let silent = dir.join("silent.ae");
    fs::write(&silent, "fn main() {}\n").unwrap();
    let empty = Command::new(cli).args(["doc", "--test"]).arg(&silent).output().unwrap();
    assert!(empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stdout).contains("no doc examples"));
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn explain_names_stable_codes_and_rejects_unknown_ones() {
    let cli = env!("CARGO_BIN_EXE_ae");
    let known = Command::new(cli).args(["explain", "e0102"]).output().unwrap();
    assert!(known.status.success(), "{}", String::from_utf8_lossy(&known.stderr));
    let text = String::from_utf8_lossy(&known.stdout);
    assert!(text.starts_with("E0102: ") && text.contains("moved"), "{text}");

    let unsafe_code = Command::new(cli).args(["explain", "E0704"]).output().unwrap();
    assert!(unsafe_code.status.success());
    assert!(String::from_utf8_lossy(&unsafe_code.stdout).contains("doc comment"));

    let unknown = Command::new(cli).args(["explain", "E9999"]).output().unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown error code 'E9999'"));
    let missing = Command::new(cli).arg("explain").output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn unsafe_fn_without_a_doc_comment_is_rejected_with_e0704() {
    let dir = unique_temp_dir("unsafe_doc_comment");
    let source = dir.join("poke.ae");
    fs::write(&source, "unsafe fn poke(v: i64) -> i64 {\n    return v * 3;\n}\nfn main() {\n    print(poke(4));\n}\n").unwrap();
    let cli = env!("CARGO_BIN_EXE_ae");
    let text = Command::new(cli).arg("check").arg(&source).output().unwrap();
    assert!(!text.status.success());
    assert!(String::from_utf8_lossy(&text.stderr).contains("doc comment"), "{}", String::from_utf8_lossy(&text.stderr));

    let json = Command::new(cli).args(["check", "--json"]).arg(&source).output().unwrap();
    let report = String::from_utf8_lossy(&json.stdout);
    assert!(!json.status.success() && report.contains("\"code\":\"E0704\""), "{report}");

    // A doc comment above the declaration clears the rule.
    fs::write(&source, "/// Caller guarantees v is a valid aligned i64; no aliasing; single-threaded.\nunsafe fn poke(v: i64) -> i64 {\n    return v * 3;\n}\nfn main() {\n    unsafe { print(poke(4)); }\n}\n").unwrap();
    let accepted = Command::new(cli).arg("check").arg(&source).output().unwrap();
    assert!(accepted.status.success(), "{}", String::from_utf8_lossy(&accepted.stderr));
    fs::remove_dir_all(dir).unwrap();
}


#[test]
fn threads_spawn_wait_and_move_captures() {
    // ADR-0214 / spec/concurrency.md T1/T7/T8: owned captures move into the
    // thread, wait hands back the spawned callable's return, handles of
    // different return types stay distinct, and a loop can spawn-and-join.
    let source = r#"fn main() {
    let base = 10;
    let offset = spawn(|| -> i64 { return base + 32; });
    print(wait(offset));
    let greet = spawn(|| -> string { return "hi " + "there"; });
    print(wait(greet));
    let total = spawn(|| -> i64 { return 5; });
    let name = spawn(|| -> string { return "done"; });
    print(wait(total) + 1);
    print(wait(name));
    var i = 0;
    var sum = 0;
    while i < 4 {
        let add = spawn(|| -> i64 { return i * 2; });
        sum = sum + wait(add);
        i = i + 1;
    }
    print(sum);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "42\nhi there\n6\ndone\n12\n"
    );

    // A `fn() -> T` function value spawns through the same lowering: the
    // pointer rides the context and the trampoline calls through it.
    let fn_value = r#"fn run_async(f: fn() -> i64) -> i64 {
    let t = spawn(f);
    return wait(t);
}
fn main() {
    print(run_async(|| -> i64 { return 41; }) + 1);
}"#;
    let output = execute(fn_value);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "42\n");

    // The interpreter runs the same program with the same observable result
    // (the spawned callable completes at the spawn site).
    let path = std::env::temp_dir().join(format!("ae-spawn-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", path.to_str().unwrap(), "--interp"]).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(
        String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"),
        "42\nhi there\n6\ndone\n12\n"
    );
}

#[test]
fn atomics_share_one_cell_across_threads() {
    // spec/concurrency.md §8 A1–A4: copying the handle shares the cell; two
    // threads adding to it lose no update; the final load is exactly the
    // sum; a store in a thread is visible to main after wait; and the old
    // values' multiset is the deterministic pre-sequence, so summing the
    // two threads' local sums is interleaving-independent.
    let source = r#"fn add_many(cell: atomic, count: i64) -> i64 {
    var i = 0;
    var seen = 0;
    while i < count {
        seen = seen + cell_add(cell, 10);
        i = i + 1;
    }
    return seen;
}
fn main() {
    let cell = cell_new(0);
    let first = spawn(|| -> i64 { return add_many(cell, 5); });
    let second = spawn(|| -> i64 { return add_many(cell, 5); });
    let lost = wait(first) + wait(second);
    print(cell_load(cell));
    print(lost);
    cell_store(cell, 7);
    let reader = spawn(|| -> i64 { return cell_load(cell); });
    print(wait(reader));
    let alias = cell;
    let shifted = cell_add(alias, 3);
    print(shifted - 3 + cell_load(cell));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // 10 adds of 10: the final load is 100; the lost-update check sums the
    // pre-values {0,10,...,90} = 450; the store-then-read handoff yields 7;
    // the alias shares the cell: the add saw 7 and made it 10.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "100\n450\n7\n14\n"
    );

    // The interpreter's single-threaded ordering produces the same values.
    let path = std::env::temp_dir().join(format!("ae-atomic-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", path.to_str().unwrap(), "--interp"]).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(
        String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"),
        "100\n450\n7\n14\n"
    );
}

#[test]
fn cancelled_receivers_leave_the_value_for_the_next_receiver() {
    // spec/concurrency.md §11 (M4.3.05): a cancellation channel only closes,
    // and a closed arm wins the select even when it is listed second. A
    // receiver that sees the cancel arm returns without consuming, so the
    // buffered value and a parked rendezvous sender's value both survive for
    // the next receiver — no duplicate delivery, no lost wakeup.
    let source = r#"fn main() {
    let stop = chan_new(1);
    let c = chan_new(1);
    print(chan_try_send(c, 41));
    chan_close(stop);
    print(chan_select(c, stop));
    print(chan_recv(c));
    let r = chan_new(0);
    let sender = spawn(|| -> bool { return chan_send(r, 7); });
    print(chan_select(r, stop));
    print(chan_recv(r));
    print(wait(sender));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "true\n1\n41\n1\n7\ntrue\n"
    );

    // The interpreter ranks a closed arm above a ready one too, so the same
    // program prints the same bytes with --interp.
    let path = std::env::temp_dir().join(format!("ae-chan-cancel-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", path.to_str().unwrap(), "--interp"]).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(
        String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"),
        "true\n1\n41\n1\n7\ntrue\n"
    );
}

#[test]
fn a_failed_child_surfaces_at_its_wait_after_the_sibling_joined() {
    // spec/concurrency.md §12 (M4.1.06): a child failure stops the failing
    // thread, not the process. The sibling joined first runs to completion, and
    // the failed handle's wait then reports the child's failure with the same
    // stderr line and exit status an immediate abort produced.
    let source = r#"fn main() {
    let sibling = spawn(|| -> i64 { print("sibling"); return 7; });
    let failing = spawn(|| -> i64 { let zero = 0; return 10 / zero; });
    print(wait(sibling));
    print(wait(failing));
    print("unreachable");
}"#;
    let output = execute(source);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert_eq!(stdout, "sibling\n7\n", "the sibling completed before the abort");
    assert_eq!(output.status.code(), Some(1), "a child failure ends the process nonzero");
    assert!(stderr.starts_with("ae:"), "the child's diagnostic keeps the runtime prefix: {stderr}");
    assert!(stderr.contains("division by zero"), "the child's message travels to the joiner: {stderr}");
    assert!(!stdout.contains("unreachable"), "the failing wait ends the process: {stdout}");

    // The interpreter stores the body's trap and replays it at wait, so the
    // same program prints the same bytes and fails at the same point.
    let path = std::env::temp_dir().join(format!("ae-thread-failure-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).args(["run", path.to_str().unwrap(), "--interp"]).output().unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"),
        "sibling\n7\n",
        "the interpreter agrees on the output ordering"
    );
    assert_eq!(interp.status.code(), Some(1));
}

#[test]
fn scoped_borrow_captures_join_before_the_region_ends() {
    // ADR-0215 (M4.1.02): a borrow capture is accepted when every path from
    // the spawn reaches a wait of the handle before any exit. The frozen
    // variable lifts... the freeze lasts to scope end (conservative), so
    // reads stay legal and writes stay rejected.
    let positive = r#"fn main() {
    var s = "a long analysis result";
    let t = spawn(|| -> i64 { return len(s); });
    print(wait(t));
    print(len(s));
}"#;
    let output = execute(positive);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "22\n22\n");

    // A loop-spawned handle joined inside the same iteration is provable.
    let joined_in_loop = r#"fn main() {
    var s = "loop body";
    var total = 0;
    var i = 0;
    while i < 3 {
        let t = spawn(|| -> i64 { return len(s) + i; });
        total = total + wait(t);
        i = i + 1;
    }
    print(total);
}"#;
    let output = execute(joined_in_loop);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "30\n");

    // The adversarial shapes: an unjoined branch exits without the wait, and
    // a wait that only runs inside a conditional body cannot cover the
    // spawn's escaping path.
    let cases = [
        ("fn main() { var s = \"hello\"; let t = spawn(|| -> i64 { return len(s); }); if 1 < 2 { print(wait(t)); } print(1); }",
         "the handle is not joined before this function exits"),
        ("fn main() { var s = \"hello\"; if 1 < 2 { let t = spawn(|| -> i64 { return len(s); }); } print(1); }",
         "the handle is not joined before this function exits"),
    ];
    for (source, expected) in cases {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}
  expected '{expected}', got '{message}'");
    }
}

#[test]
fn thread_boundaries_reject_nontransferable_shapes() {
    // spec/concurrency.md T2/T3/T5/T6: a borrow capture cannot cross, the
    // callable takes no visible parameters, a value capture moves (use after
    // spawn is E0102), and the handle is consumed by wait.
    let cases = [
        // T2: the borrow probe classifies `s` as a string view — rejected.
        ("fn main() { var s = \"hello\"; let t = spawn(|| -> i64 { return len(s); }); print(1); }",
         "the handle is not joined before this function exits"),
        // T3: a spawned callable takes no visible parameters.
        ("fn main() { let t = spawn(|x: i64| -> i64 { return x; }); print(1); }",
         "the spawned closure must take no parameters"),
        // T4: spawn needs a closure value.
        ("fn main() { let t = spawn(42); print(1); }",
         "spawn needs a closure value"),
        // T5: the value capture moved at the spawn site.
        ("fn main() { var s = \"hello\"; let t = spawn(|| -> string { return s; }); print(s); }",
         "because it was moved"),
        // T6: wait consumes the handle.
        ("fn main() { let t = spawn(|| -> i64 { return 1; }); let a = wait(t); let b = wait(t); print(a + b); }",
         "because it was moved"),
    ];
    for (source, expected) in cases {
        let message = check_rejects(source);
        assert!(message.contains(expected), "{source}\n  expected '{expected}', got '{message}'");
    }
}

#[test]
fn tasks_fifo_yield_and_reclaim_agree_across_backends() {
    // M4.2.01–M4.2.03 (ADR-0218): compiled C (Win32 Fibers / POSIX C11) and
    // --interp must produce byte-for-byte identical deterministic FIFO output
    // across yields and 200 reclaimed tasks.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/tasks_fifo.ae");
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    let c_out = String::from_utf8_lossy(&compiled.stdout).replace("\r\n", "\n");
    let i_out = String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n");
    assert_eq!(c_out, i_out);
    assert_eq!(c_out, "k1:1\nk1:2\nk1:3\n3\n0\n10\n1\n11\n2\n12\n2\n200\n200\n");
}

#[test]
fn fake_clock_and_timers_agree_across_backends() {
    // ADR-0224 R2.2: the fake clock lives in std/std/test/fake_clock.ae; equal
    // deadlines, pre/post-deadline cancellation, and task-driven timeout must
    // match byte-for-byte between compiled C and --interp (pinned to the
    // former builtin implementation's output).
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/fake_clock.ae");
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    let c_out = String::from_utf8_lossy(&compiled.stdout).replace("\r\n", "\n");
    let i_out = String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n");
    assert_eq!(c_out, i_out);
    assert_eq!(c_out, "0\nfalse\nfalse\ntrue\nfalse\n50\ntrue\ntrue\nfalse\n110\nfalse\n2\ntimeout_fired\n2\n140\n");
}

#[test]
fn task_scope_cancel_and_work_stealing_agree_across_backends() {
    // M4.2.04 / M4.2.05 / M4.2.07 (ADR-0220): structured task scope join on
    // early return, one-terminal-outcome cancellation tokens, and 4-worker
    // work-stealing pool executing 100 tasks exactly once must match between
    // compiled C and --interp.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/tasks_advanced.ae");
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    let c_out = String::from_utf8_lossy(&compiled.stdout).replace("\r\n", "\n");
    let i_out = String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n");
    assert_eq!(c_out, i_out);
    assert_eq!(c_out, "7\n30\ntrue\nfalse\ntrue\ntrue\nfalse\nfalse\ntrue\n2\n2\n100\n100\n");
}

#[test]
fn byte_streams_and_buffered_readers_agree_across_backends() {
    // M3.2.01 / M3.2.02 (ADR-0221): scripted short-chunk byte stream + buffered
    // line reader spanning multiple buffers while preserving the following byte
    // must match byte-for-byte between compiled C and --interp.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/byte_streams.ae");
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    let c_out = String::from_utf8_lossy(&compiled.stdout).replace("\r\n", "\n");
    let i_out = String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n");
    assert_eq!(c_out, i_out);
    assert_eq!(c_out, "abc\ndef\ng\n0\ntrue\n0123456789\nZ\ntail\ntrue\n");
}

#[test]
fn fake_io_std_module_agrees_across_backends() {
    // ADR-0224 R2.1: the in-memory readiness/completion model lives in
    // std/std/test/fake_io.ae and agrees byte-for-byte across backends.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/fake_io.ae");
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["run", fixture.to_str().unwrap(), "--interp"])
        .output()
        .unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    let c_out = String::from_utf8_lossy(&compiled.stdout).replace("\r\n", "\n");
    let i_out = String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n");
    assert_eq!(c_out, i_out);
    let expected = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run/fake_io.stdout")).unwrap().replace("\r\n", "\n");
    assert_eq!(c_out, expected);
}

/// R3b differential gate: the resolver's name model must agree with the
/// compiler on every accepted program. Three claims, checked per file:
/// the shared grammar parses what the compiler accepts, the resolver finds
/// no duplicate-variable or unknown-target diagnostics, and every write
/// target or borrow argument names a live local. Programs the compiler
/// rejects stay out of the gate — the resolver keeps dumping those.
#[test]
fn resolver_agrees_with_the_compiler_on_accepted_programs() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for dir in ["tests/run", "examples"] {
        let mut stack = vec![root.join(dir)];
        while let Some(path) = stack.pop() {
            let Ok(entries) = fs::read_dir(&path) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "ae") {
                    continue;
                }
                let label = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                let source = fs::read_to_string(&path).unwrap_or_default();
                // Only programs the compiler itself accepts belong in this gate.
                let verdict = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&path).output().unwrap();
                if !verdict.status.success() {
                    continue;
                }
                // Imports are followed the way the CLI does: the imports'
                // pub items become visible, so an accepted multi-file
                // program resolves without unknowns.
                let resolution = match ae_resolve::load_and_resolve(&path) {
                    Ok((_, resolution)) => resolution,
                    Err(error) => {
                        failures.push(format!("{label}: the resolver cannot process an accepted program: {error}"));
                        continue;
                    }
                };
                let _ = &source;
                for diagnostic in &resolution.diagnostics {
                    failures.push(format!("{label}: {}", diagnostic.message));
                }
                for function in &resolution.functions {
                    for occurrence in &function.occurrences {
                        if matches!(occurrence.role, ae_resolve::Role::Write | ae_resolve::Role::Borrow) && occurrence.target.is_none() {
                            failures.push(format!("{label}: write or borrow of '{}' resolves to nothing", occurrence.name));
                        }
                    }
                }
                // ADR-0228: every type spelling of an accepted program
                // classifies — an unknown name would mean the compiler
                // accepted a type the resolver cannot see (an imported
                // type, or a drift bug).
                for entry in &resolution.spellings {
                    if entry.spelled.spelling.is_unknown() {
                        failures.push(format!("{label}: type spelling of '{}' is unknown", entry.name));
                    }
                }
                checked += 1;
            }
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
    assert!(checked > 100, "expected a large resolution corpus, checked {checked}");
}

// P1.4: a body the resolved route does not take is a diagnostic in every
// mode (the typed parser and its escape hatch are gone). A fully routed
// program compiles; an unrouted statement shape fails with the message.
#[test]
fn unrouted_bodies_are_diagnostics() {
    let routed = "struct S { a: i64, b: i64 }\nfn make(a: i64, b: i64) -> S { return S { a: a, b: b }; }\nfn main() { let s = make(1, 2); print(s.a + s.b); }\n";
    // A bare method call statement (`p.m();`) is still off the resolved
    // route, so it is rejected instead of falling back.
    let legacy = "struct P { n: i64 } impl P { fn m(self) -> i64 { return self.n; } } fn main() { let p = P { n: 1 }; p.m(); }\n";
    for (source, expect_ok) in [(routed, true), (legacy, false)] {
        let path = std::env::temp_dir().join(format!(
            "ae-deny-legacy-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(path).unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), expect_ok, "{stderr}");
        if !expect_ok {
            assert!(stderr.contains("the legacy body parser"), "{stderr}");
        }
    }
}

// P1.1 chained-call rung: a method call on the result of another call
// (`start.shifted(1, 2).shifted(0, -6)`) lowers on the resolved route. The
// binding's type comes from the chain's declared returns, so the following
// `moved.describe()` resolves too, and the comparison around it keeps its
// operand bound (`...value() == 11`). The compiled and interpreted runs
// agree byte for byte, and P1.4 leaves no fallback path to take.
#[test]
fn chained_method_calls_lower_and_run_on_the_resolved_route() {
    let source = r#"struct Point {
    x: i64,
    y: i64,
}
impl Point {
    fn new(x: i64, y: i64) -> Point {
        return Point { x: x, y: y };
    }
    fn shifted(self, dx: i64, dy: i64) -> Point {
        return Point { x: self.x + dx, y: self.y + dy };
    }
    fn distance(self) -> i64 {
        return abs_i64(self.x) + abs_i64(self.y);
    }
    fn describe(self) -> string {
        return "(" + to_string(self.x) + ", " + to_string(self.y) + ")";
    }
}
fn abs_i64(value: i64) -> i64 {
    if value < 0 {
        return -value;
    }
    return value;
}
fn main() {
    let start = Point::new(3, 4);
    let moved = start.shifted(1, 2).shifted(0, -6);
    print(moved.describe());
    let from_origin = Point::new(1, 2);
    assert(from_origin.distance() == 3);
    print(1);
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "(4, 0)\n1\n");

    // The same source runs identically under the interpreter.
    let path = std::env::temp_dir().join(format!("ae-chained-methods-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("run")
        .arg(&path)
        .arg("--interp")
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"), "(4, 0)\n1\n");

    // Every body is on the resolved route; check accepts it.
    let path = std::env::temp_dir().join(format!("ae-chained-methods-check-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let check = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
}

// P1.1 pointer rung: the extern allocator bridge, pointer retag, pointer
// arithmetic (`offset`/`byte_offset`), stores through `*p`, `deref` reads and
// the free call all lower on the resolved route. The check run proves the
// whole body is resolved-route; the compiled output is the fixture's.
#[test]
fn raw_pointer_surface_lowers_on_the_resolved_route() {
    let source = r#"extern "C" fn ae_std_alloc(size: i64, line: i64, column: i64) -> *mut i64;
/// Preconditions: block came from ae_std_alloc; released exactly once.
extern "C" fn ae_std_free(block: *mut i64) -> *mut i64;

fn main() {
    unsafe {
        let buf = ae_std_alloc(64, 12, 22);
        *buf = 10;
        let second = buf.offset(1);
        *second = 20;
        let third = buf.byte_offset(16);
        *third = 30;
        print(buf.deref());
        print(second.deref());
        print(third.deref());
        let view = buf as *const f64;
        print("retagged");
        let freed = ae_std_free(buf);
    }
    print("done");
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "10\n20\n30\nretagged\ndone\n"
    );

    // Every body is on the resolved route; check accepts it.
    let path = std::env::temp_dir().join(format!("ae-pointer-route-check-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let check = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
}

// P1.1 drop rung: a construction of a struct that declares `drop` lowers on
// the resolved route, and the user's `drop` runs on every exit path (the
// early return and the falling exit), exactly where the typed route ran it.
#[test]
fn drop_carrying_constructions_lower_on_the_resolved_route() {
    let source = r#"struct Guard {
    id: i64,
}
impl Guard {
    fn drop(&mut self) -> i64 {
        print(self.id);
        return 0;
    }
}
fn early(flag: bool) -> i64 {
    let g = Guard { id: 7 };
    if flag {
        return 1;
    }
    return 2;
}
fn main() {
    print(early(true));
    print(early(false));
}"#;
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "7\n1\n7\n2\n");

    // Every body is on the resolved route; check accepts it.
    let path = std::env::temp_dir().join(format!("ae-drop-route-check-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let check = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
}

// P1.4 slice 2: enum constructions lower from the shared syntax tree. The
// payload-free path (`Shape::Empty`) and the payload path (`Shape::Circle(3)`)
// both go through lower_shared_enum_construction, so the route report for
// this program carries no typed-expression fallback line.
#[test]
fn enum_constructions_lower_on_the_resolved_route() {
    let source = r#"enum Shape {
    Circle(i64),
    Empty,
}

fn area(shape: Shape) -> i64 {
    match shape {
        Shape::Circle(r) => { return r * r; }
        Shape::Empty => { return 0; }
    }
}

fn pick(flag: bool) -> Option[i64] {
    if flag { return Option[i64]::Some(7); }
    return Option[i64]::None();
}

fn main() {
    let circle = Shape::Circle(3);
    print(area(circle));
    let empty = Shape::Empty;
    print(area(empty));
    let picked = pick(true);
    match picked {
        Option::Some(value) => { print(value); }
        Option::None => { print(0); }
    }
}"#;
    let path = std::env::temp_dir().join(format!("ae-enum-construction-route-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .env("AE_ROUTE_REPORT", "1")
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("route typed-expression"), "{stderr}");
}

// P2.1 (ADR-0232): `as` converts between i64 and f64 explicitly. Int->float
// rounds, float->int truncates and traps outside the i64 range, and the
// compiled and interpreted runs agree byte for byte; a non-numeric target is
// a compile-time diagnostic.
#[test]
fn scalar_as_casts_match_between_backends_and_trap_alike() {
    let source = "fn main() {\n    let x = 3;\n    print(x as f64);\n    let z = 2.5 as i64;\n    print(z);\n    print(1 as f64 + 0.5);\n}";
    let output = execute(source);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"), "3.0\n2\n1.5\n");

    let path = std::env::temp_dir().join(format!("ae-scalar-casts-interp-{}.ae", std::process::id()));
    fs::write(&path, source).unwrap();
    let interp = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--interp").output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(interp.status.success(), "{}", String::from_utf8_lossy(&interp.stderr));
    assert_eq!(String::from_utf8_lossy(&interp.stdout).replace("\r\n", "\n"), "3.0\n2\n1.5\n");

    let trap = "fn main() {\n    let big = 1e300;\n    print(big as i64);\n}";
    for arguments in [vec![], vec!["--interp"]] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run");
        let path = std::env::temp_dir().join(format!("ae-scalar-cast-trap-{}-{}.ae", std::process::id(), arguments.len()));
        fs::write(&path, trap).unwrap();
        command.arg(&path);
        for argument in &arguments { command.arg(argument); }
        let output = command.output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("float-to-integer cast out of range"));
    }

    let message = check_rejects("fn main() { let b = 3 as bool; print(b); }");
    assert!(message.contains("conversion from i64 to bool is not allowed"), "{message}");
}

#[test]
fn i8_u8_compiled_and_interp_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/i8_u8_primitives.ae");
    let expected = fs::read_to_string(root.join("tests/run/i8_u8_primitives.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for extra in [vec![], vec!["--interp"]] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {:?} failed: {}",
            extra,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {extra:?}"
        );
    }

    let neg_i8 = check_rejects("fn main() { let x = (-129) as i8; print(x); }");
    assert!(neg_i8.contains("out of range for 'i8'"), "{neg_i8}");
    let neg_u8 = check_rejects("fn main() { let x = (-1) as u8; print(x); }");
    assert!(neg_u8.contains("out of range for 'u8'"), "{neg_u8}");
}

#[test]
fn i8_u8_runtime_traps_match_between_backends() {
    let cases = [
        ("i8_add_overflow", "fn main() { let a = 127 as i8; let b = 1 as i8; print(a + b); }", "integer overflow"),
        ("i8_sub_underflow", "fn main() { let a = (-128) as i8; let b = 1 as i8; print(a - b); }", "integer overflow"),
        ("i8_neg_min", "fn main() { let a = (-128) as i8; print(-a); }", "integer overflow"),
        ("i8_div_min_neg1", "fn main() { let a = (-128) as i8; let b = (-1) as i8; print(a / b); }", "integer overflow"),
        ("u8_add_overflow", "fn main() { let a = 255 as u8; let b = 1 as u8; print(a + b); }", "integer overflow"),
        ("u8_sub_underflow", "fn main() { let a = 0 as u8; let b = 1 as u8; print(a - b); }", "integer overflow"),
        ("i8_div_zero", "fn main() { let a = 1 as i8; let b = 0 as i8; print(a / b); }", "division by zero"),
        ("i8_mod_zero", "fn main() { let a = 1 as i8; let b = 0 as i8; print(a % b); }", "division by zero"),
        ("u8_div_zero", "fn main() { let a = 1 as u8; let b = 0 as u8; print(a / b); }", "division by zero"),
        ("u8_mod_zero", "fn main() { let a = 1 as u8; let b = 0 as u8; print(a % b); }", "division by zero"),
        ("i8_shl_oob", "fn main() { let a = 1 as i8; let b = 8 as i8; print(a << b); }", "shift count out of range"),
        ("i8_shr_neg", "fn main() { let a = 1 as i8; let b = (-1) as i8; print(a >> b); }", "shift count out of range"),
        ("u8_shl_oob", "fn main() { let a = 1 as u8; let b = 8 as u8; print(a << b); }", "shift count out of range"),
        ("u8_shr_oob", "fn main() { let a = 1 as u8; let b = 8 as u8; print(a >> b); }", "shift count out of range"),
        ("f64_to_i8_nan", "fn main() { let x = sqrt(-1.0); print(x as i8); }", "float-to-integer cast out of range"),
        ("f64_to_u8_inf", "fn main() { let x = exp(1000.0); print(x as u8); }", "float-to-integer cast out of range"),
        ("f64_to_i8_high", "fn main() { let x = 128.0; print(x as i8); }", "float-to-integer cast out of range"),
        ("f64_to_i8_low", "fn main() { let x = -129.0; print(x as i8); }", "float-to-integer cast out of range"),
        ("f64_to_u8_high", "fn main() { let x = 256.0; print(x as u8); }", "float-to-integer cast out of range"),
        ("f64_to_u8_low", "fn main() { let x = -1.0; print(x as u8); }", "float-to-integer cast out of range"),
    ];

    let pid = std::process::id();
    for (name, source, expected_trap) in cases {
        for arguments in [vec![], vec!["--interp"]] {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ae-i8-u8-trap-{name}-{pid}-{seq}.ae"));
            fs::write(&path, source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
            command.arg("run").arg(&path);
            for arg in &arguments {
                command.arg(arg);
            }
            let output = command.output().unwrap();
            let _ = fs::remove_file(&path);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{name} ({arguments:?}) unexpectedly succeeded"
            );
            assert!(
                stderr.contains(expected_trap),
                "{name} ({arguments:?}) stderr {stderr:?} missing {expected_trap:?}"
            );
        }
    }
}

#[test]
fn all_int_widths_compiled_mir_and_interp_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/int_widths_primitives.ae");
    let expected = fs::read_to_string(root.join("tests/run/int_widths_primitives.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }

    for (src, needle) in [
        ("fn main() { let x = (-32769) as i16; print(x); }", "out of range for 'i16'"),
        ("fn main() { let x = (-1) as u16; print(x); }", "out of range for 'u16'"),
        ("fn main() { let x = (-1) as u32; print(x); }", "out of range for 'u32'"),
        ("fn main() { let x = (-1) as usize; print(x); }", "out of range for 'usize'"),
        ("fn main() { let x = 1 as u32; print(-x); }", "unary '-' is not allowed on unsigned"),
        ("fn main() { let x = 1 as u64; print(-x); }", "unary '-' is not allowed on unsigned"),
        ("fn main() { let x = 1 as usize; print(-x); }", "unary '-' is not allowed on unsigned"),
    ] {
        let msg = check_rejects(src);
        assert!(msg.contains(needle), "{src}: {msg}");
    }
}

#[test]
fn all_int_widths_runtime_traps_match_between_backends() {
    let cases = [
        ("i16_add_ovf", "fn main() { let a = 32767 as i16; print(a + (1 as i16)); }", "integer overflow"),
        ("i16_neg_min", "fn main() { let a = (-32768) as i16; print(-a); }", "integer overflow"),
        ("i16_div_zero", "fn main() { let a = 1 as i16; print(a / (0 as i16)); }", "division by zero"),
        ("i16_shl_16", "fn main() { let a = 1 as i16; print(a << (16 as i16)); }", "shift count out of range"),
        ("u16_add_ovf", "fn main() { let a = 65535 as u16; print(a + (1 as u16)); }", "integer overflow"),
        ("u16_sub_udf", "fn main() { let a = 0 as u16; print(a - (1 as u16)); }", "integer overflow"),
        ("u16_shr_16", "fn main() { let a = 1 as u16; print(a >> (16 as u16)); }", "shift count out of range"),
        ("i32_add_ovf", "fn main() { let a = 2147483647 as i32; print(a + (1 as i32)); }", "integer overflow"),
        ("i32_div_min", "fn main() { let a = (-2147483648) as i32; print(a / ((-1) as i32)); }", "integer overflow"),
        ("i32_shl_neg", "fn main() { let a = 1 as i32; print(a << ((-1) as i32)); }", "shift count out of range"),
        ("u32_add_ovf", "fn main() { let a = 4294967295 as u32; print(a + (1 as u32)); }", "integer overflow"),
        ("u32_mod_zero", "fn main() { let a = 1 as u32; print(a % (0 as u32)); }", "division by zero"),
        ("u32_shl_32", "fn main() { let a = 1 as u32; print(a << (32 as u32)); }", "shift count out of range"),
        ("isize_add_ovf", "fn main() { let a = 9223372036854775807 as isize; print(a + (1 as isize)); }", "integer overflow"),
        ("isize_div_zero", "fn main() { let a = 1 as isize; print(a / (0 as isize)); }", "division by zero"),
        ("isize_shr_64", "fn main() { let a = 1 as isize; print(a >> (64 as isize)); }", "shift count out of range"),
        ("u64_add_ovf", "fn main() { let h = (1 as u64) << (63 as u64); let m = h + (h - (1 as u64)); print(m + (1 as u64)); }", "integer overflow"),
        ("u64_sub_udf", "fn main() { let a = 0 as u64; print(a - (1 as u64)); }", "integer overflow"),
        ("u64_mod_zero", "fn main() { let a = 1 as u64; print(a % (0 as u64)); }", "division by zero"),
        ("u64_shl_64", "fn main() { let a = 1 as u64; print(a << (64 as u64)); }", "shift count out of range"),
        ("usize_sub_udf", "fn main() { let a = 0 as usize; print(a - (1 as usize)); }", "integer overflow"),
        ("usize_shr_64", "fn main() { let a = 1 as usize; print(a >> (64 as usize)); }", "shift count out of range"),
        ("f64_to_i16_high", "fn main() { let x = 32768.0; print(x as i16); }", "float-to-integer cast out of range"),
        ("f64_to_i16_low", "fn main() { let x = -32769.0; print(x as i16); }", "float-to-integer cast out of range"),
        ("f64_to_u16_high", "fn main() { let x = 65536.0; print(x as u16); }", "float-to-integer cast out of range"),
        ("f64_to_u16_low", "fn main() { let x = -1.0; print(x as u16); }", "float-to-integer cast out of range"),
        ("f64_to_i32_high", "fn main() { let x = 2147483648.0; print(x as i32); }", "float-to-integer cast out of range"),
        ("f64_to_u32_high", "fn main() { let x = 4294967296.0; print(x as u32); }", "float-to-integer cast out of range"),
        ("f64_to_isize_high", "fn main() { let x = 9223372036854775808.0; print(x as isize); }", "float-to-integer cast out of range"),
        ("f64_to_u64_high", "fn main() { let x = 18446744073709551616.0; print(x as u64); }", "float-to-integer cast out of range"),
        ("f64_to_usize_nan", "fn main() { let x = sqrt(-1.0); print(x as usize); }", "float-to-integer cast out of range"),
        ("f64_to_usize_inf", "fn main() { let x = exp(1000.0); print(x as usize); }", "float-to-integer cast out of range"),
    ];

    let pid = std::process::id();
    for (name, source, expected_trap) in cases {
        for arguments in [vec![], vec!["--interp"]] {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ae-w2-trap-{name}-{pid}-{seq}.ae"));
            fs::write(&path, source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
            command.arg("run").arg(&path);
            for arg in &arguments {
                command.arg(arg);
            }
            let output = command.output().unwrap();
            let _ = fs::remove_file(&path);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{name} ({arguments:?}) unexpectedly succeeded"
            );
            assert!(
                stderr.contains(expected_trap),
                "{name} ({arguments:?}) stderr {stderr:?} missing {expected_trap:?}"
            );
        }
    }
}

#[test]
fn wave3a_f32_char_unit_ptr_usize_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/f32_char_unit_ptr_usize.ae");
    let expected = fs::read_to_string(root.join("tests/run/f32_char_unit_ptr_usize.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }

    let mut_write_src = "fn main() { var y = 10; unsafe { let p = &mut y as *mut i64; let a = p as usize; let q = a as *mut i64; let _w = ptr_write(q, 99); } print(y); }\n";
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ae-w3a-mut-ptr-{}-{seq}.ae", std::process::id()));
    fs::write(&path, mut_write_src).unwrap();
    for (label, mir) in [("compiled", false), ("mir", true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&path);
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{label} mut ptr round-trip failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            "99\n"
        );
    }
    let _ = fs::remove_file(&path);

    let msg = check_rejects("fn main() { let x = 65 as char; print(x); }");
    assert!(msg.contains("not allowed"), "{msg}");
}

#[test]
fn wave3a_runtime_traps_match_between_backends() {
    let cases = [
        ("f32_to_i8_nan", "fn main() { let x = sqrt(-1.0) as f32; print(x as i8); }", "float-to-integer cast out of range"),
        ("f32_to_u8_inf", "fn main() { let x = exp(1000.0) as f32; print(x as u8); }", "float-to-integer cast out of range"),
        ("f32_to_i8_high", "fn main() { let x = 128.0 as f32; print(x as i8); }", "float-to-integer cast out of range"),
        ("f32_to_u8_low", "fn main() { let x = (-1.0) as f32; print(x as u8); }", "float-to-integer cast out of range"),
        ("f32_to_i32_high", "fn main() { let x = 2147483648.0 as f32; print(x as i32); }", "float-to-integer cast out of range"),
        ("f32_to_u32_high", "fn main() { let x = 4294967296.0 as f32; print(x as u32); }", "float-to-integer cast out of range"),
        ("f32_to_isize_high", "fn main() { let x = 9223372036854775808.0 as f32; print(x as isize); }", "float-to-integer cast out of range"),
        ("f32_to_usize_high", "fn main() { let x = 18446744073709551616.0 as f32; print(x as usize); }", "float-to-integer cast out of range"),
        ("f32_div_zero", "fn main() { let a = 1.0 as f32; let b = 0.0 as f32; print(a / b); }", "f32 division by zero"),
        ("u32_to_char_d800", "fn main() { let v = 55296 as u32; print(v as char); }", "invalid Unicode scalar value"),
        ("u32_to_char_dfff", "fn main() { let v = 57343 as u32; print(v as char); }", "invalid Unicode scalar value"),
        ("u32_to_char_110000", "fn main() { let v = 1114112 as u32; print(v as char); }", "invalid Unicode scalar value"),
    ];

    let pid = std::process::id();
    for (name, source, expected_trap) in cases {
        for arguments in [vec![], vec!["--interp"]] {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ae-w3a-trap-{name}-{pid}-{seq}.ae"));
            fs::write(&path, source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
            command.arg("run").arg(&path);
            for arg in &arguments {
                command.arg(arg);
            }
            let output = command.output().unwrap();
            let _ = fs::remove_file(&path);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{name} ({arguments:?}) unexpectedly succeeded"
            );
            assert!(
                stderr.contains(expected_trap),
                "{name} ({arguments:?}) stderr {stderr:?} missing {expected_trap:?}"
            );
        }
    }
}

#[test]
fn wave3b_int_checked_wrapping_saturating_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/int_checked_wrapping_saturating.ae");
    let expected = fs::read_to_string(root.join("tests/run/int_checked_wrapping_saturating.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

#[test]
fn p2_2_copy_derive_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/copy_derive.ae");
    let expected = fs::read_to_string(root.join("tests/run/copy_derive.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

#[test]
fn p2_2_drop_order_and_trait_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/drop_order_and_trait.ae");
    let expected = fs::read_to_string(root.join("tests/run/drop_order_and_trait.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }

    // Inherent unit `fn drop(&mut self)` + multi-field declaration-order drops.
    let multi_field_src = "struct Leaf { id: i64 }\nimpl Leaf { fn drop(&mut self) { print(\"leaf \" + to_string(self.id)); } }\nstruct Pair { a: Leaf, b: Leaf }\nimpl Drop for Pair { fn drop(&mut self) { print(\"pair\"); } }\nfn main() { let l1 = Leaf { id: 1 }; let l2 = Leaf { id: 2 }; let p = Pair { a: l1, b: l2 }; }\n";
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ae-p22-drop-fields-{}-{seq}.ae", std::process::id()));
    fs::write(&path, multi_field_src).unwrap();
    for (label, extra, mir) in [
        ("compiled", vec![], false),
        ("mir", vec![], true),
        ("interp", vec!["--interp"], false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&path);
        for arg in &extra {
            command.arg(arg);
        }
        if mir {
            command.env("AE_CODEGEN", "mir");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{label} multi-field drop failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            "pair\nleaf 1\nleaf 2\n",
            "{label} multi-field drop order mismatch"
        );
    }
    let _ = fs::remove_file(&path);

    for (src, needle) in [
        ("struct G { id: i64 }\nimpl Drop for G { fn drop(&mut self) { print(self.id); } }\nfn main() { var g = G { id: 1 }; let _r = G::drop(&mut g); }", "drop"),
        ("struct G { id: i64 }\nimpl Drop for G { fn drop(&mut self, extra: i64) { print(self.id); } }\nfn main() {}", "drop"),
        ("struct G { id: i64 }\nimpl Drop for G { fn drop(&self) { print(self.id); } }\nfn main() {}", "drop"),
    ] {
        let msg = check_rejects(src);
        assert!(msg.contains(needle), "{src}: {msg}");
    }
}

#[test]
fn p2_3_storable_fn_ptrs_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/storable_fn_ptrs.ae");
    let expected = fs::read_to_string(root.join("tests/run/storable_fn_ptrs.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

#[test]
fn p2_3_first_class_closures_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/first_class_closures.ae");
    let expected = fs::read_to_string(root.join("tests/run/first_class_closures.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

/// P2.4 slice 1 (ADR-0233): the prelude's generic `Result[T, E]` is usable
/// without a declaration or import, and every payload family the
/// generic-enum rule admits agrees across the C emitters and `--interp`.
#[test]
fn p2_4_prelude_result_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/prelude_result.ae");
    let expected = fs::read_to_string(root.join("tests/run/prelude_result.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

/// P2.4 slice 2 (ADR-0233): the flat-payload preset result families are the
/// `Result[p, string]` instantiations. Both spellings must name one type and
/// agree across the C emitters and `--interp`.
#[test]
fn p2_4_preset_result_alias_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/preset_result_aliases.ae");
    let expected = fs::read_to_string(root.join("tests/run/preset_result_aliases.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

/// P2.4 slice 3 (ADR-0233): `IntResult` and `WriteResult` share the
/// `Result[i64, string]` instantiation, and the family names stay usable as
/// pattern aliases on either spelling.
#[test]
fn p2_4_preset_result_int_alias_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/preset_result_int_alias.ae");
    let expected = fs::read_to_string(root.join("tests/run/preset_result_int_alias.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

/// P2.4 slice 4 (ADR-0233): `StatusResult` is `Result[(), string]`; the unit
/// payload takes no bindings, and both the constructor and the pattern form
/// agree across the C emitters and `--interp`.
#[test]
fn p2_4_preset_result_unit_alias_parity() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/run/preset_result_unit_alias.ae");
    let expected = fs::read_to_string(root.join("tests/run/preset_result_unit_alias.stdout"))
        .unwrap()
        .replace("\r\n", "\n");

    for (label, extra, codegen) in [
        ("compiled", vec![], None),
        ("mir", vec![], Some("mir")),
        ("tree", vec![], Some("tree")),
        ("interp", vec!["--interp"], None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        for arg in &extra {
            command.arg(arg);
        }
        if let Some(mode) = codegen {
            command.env("AE_CODEGEN", mode);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            expected,
            "stdout mismatch for mode {label}"
        );
    }
}

/// ADR-0235 Slice 3a: generic fixed-size arrays `[T; N]` for Copy non-borrow,
/// non-unit types (`f64`, `bool`, `i32`, `char`, `#[derive(Copy)]` structs)
/// compile, run, and match `--interp`, while heterogeneous or non-Copy arrays
/// are rejected.
#[test]
fn generic_copy_arrays_compile_run_and_match_interpreter() {
    let source = r#"
#[derive(Copy)]
struct Point {
    x: i64,
    y: i64,
}

fn sum_floats(xs: &[f64; 3]) -> f64 {
    return xs[0] + xs[1] + xs[2];
}

fn second_point_x(pts: &[Point; 2]) -> i64 {
    let p = pts[1];
    return p.x + p.y;
}

fn main() {
    var floats = [1.5, 2.5, 3.0];
    floats[1] = 4.25;
    print(floats[0]);
    print(floats[1]);
    print(len(floats));
    print(sum_floats(&floats));

    var flags = [true, false];
    flags[1] = true;
    print(flags[0]);
    print(flags[1]);
    print(len(flags));

    var ints = [10 as i32, 20 as i32, 30 as i32];
    ints[2] = 42 as i32;
    print(ints[0]);
    print(ints[2]);
    print(len(ints));

    var chars = ['a', 'z'];
    chars[0] = 'q';
    print(chars[0]);
    print(chars[1]);
    print(len(chars));

    var pts = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];
    pts[1] = Point { x: 10, y: 20 };
    let p0 = pts[0];
    print(p0.x);
    print(second_point_x(&pts));
    print(len(pts));
}
"#;
    let path = std::env::temp_dir().join(format!(
        "ae-generic-arrays-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, source).unwrap();

    let mut outputs = Vec::new();
    for (label, extra) in [("compiled", vec![]), ("interp", vec!["--interp"])] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&path);
        for arg in &extra {
            command.arg(arg);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"));
    }
    fs::remove_file(&path).unwrap();
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        outputs[0],
        "1.5\n4.25\n3\n8.75\ntrue\ntrue\n2\n10\n42\n3\nq\nz\n2\n1\n30\n2\n"
    );

    for bad_source in [
        "fn main() { let a = [1, 2.5]; }",
        "fn main() { let a = [\"a\", \"b\"]; }",
        "fn f(a: &[string; 2]) -> i64 { return len(a); } fn main() {}",
    ] {
        let bad_path = std::env::temp_dir().join(format!(
            "ae-generic-arrays-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&bad_path, bad_source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&bad_path)
            .output()
            .unwrap();
        fs::remove_file(&bad_path).unwrap();
        assert!(!result.status.success(), "expected failure for: {bad_source}");
    }
}

/// ADR-0235 Slice 3b: generic `vec[T]` (`ae_gvec`) for Copy non-borrow,
/// non-unit types (`bool`, `i32`, `u8`, `char`, `f32`, `#[derive(Copy)]`
/// structs) compiles, runs, and matches `--interp`, while mixed or non-Copy
/// element vectors are rejected.
#[test]
fn generic_copy_vectors_compile_run_and_match_interpreter() {
    let source = r#"
#[derive(Copy)]
struct Point {
    x: i64,
    y: i64,
}

fn sum_points(pts: &vec[Point]) -> i64 {
    var total = 0;
    for pt in pts {
        total += pt.x + pt.y;
    }
    return total;
}

fn take_points(pts: vec[Point]) -> i64 {
    let p = pts[0];
    return p.x + len(pts);
}

fn main() {
    var flags = vec[true, false];
    push(flags, true);
    flags[1] = true;
    print(flags[0]);
    print(flags[1]);
    print(len(flags));
    pop(flags);
    print(len(flags));
    for f in flags {
        print(f);
    }

    var i32s = vec[10 as i32, 20 as i32];
    push(i32s, 30 as i32);
    i32s[1] = 25 as i32;
    print(i32s[1]);
    print(len(i32s));
    pop(i32s);
    print(len(i32s));

    var octets = vec[1 as u8, 2 as u8];
    push(octets, 3 as u8);
    octets[0] = 9 as u8;
    print(octets[0]);
    print(len(octets));

    var chars = vec['a', 'b'];
    push(chars, 'c');
    chars[1] = 'z';
    print(chars[1]);
    print(len(chars));

    var f32s = vec[1.5 as f32, 2.5 as f32];
    push(f32s, 3.5 as f32);
    f32s[0] = 4.5 as f32;
    print(f32s[0]);
    print(len(f32s));

    var pts = vec[Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];
    push(pts, Point { x: 5, y: 6 });
    pts[1] = Point { x: 10, y: 20 };
    let p1 = pts[1];
    print(p1.x);
    print(sum_points(&pts));
    pop(pts);
    print(take_points(pts));
}
"#;
    let path = std::env::temp_dir().join(format!(
        "ae-generic-vectors-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, source).unwrap();

    let mut outputs = Vec::new();
    for (label, extra) in [("compiled", vec![]), ("interp", vec!["--interp"])] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&path);
        for arg in &extra {
            command.arg(arg);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"));
    }
    fs::remove_file(&path).unwrap();
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        outputs[0],
        "true\ntrue\n3\n2\ntrue\ntrue\n25\n3\n2\n9\n3\nz\n3\n4.5\n3\n10\n44\n3\n"
    );

    for bad_source in [
        "fn main() { let v = vec[true, 1]; }",
        "enum OwnedEnum { Item(string) } fn main() { let v = vec[OwnedEnum::Item(\"a\")]; }",
    ] {
        let bad_path = std::env::temp_dir().join(format!(
            "ae-generic-vectors-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&bad_path, bad_source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&bad_path)
            .output()
            .unwrap();
        fs::remove_file(&bad_path).unwrap();
        assert!(!result.status.success(), "expected failure for: {bad_source}");
    }
}


/// ADR-0235 route coverage: a field read on an indexed aggregate
/// (`v[i].x`, `v[i].inner.radius`) resolves the element type from the
/// vector/array interner and lowers on every backend. A field *store*
/// through an index has no lowering yet, so it stays an honest refusal.
#[test]
fn indexed_element_field_reads_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("vector_element_field_reads.ae");
    let expected = fs::read_to_string(fixture.with_extension("stdout")).unwrap();
    let expected = lines(expected.as_bytes());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    let bad_source = r#"#[derive(Copy)] struct P { x: i64 } fn main() { var v = vec[P { x: 1 }]; v[0].x = 2; print(v[0].x); }"#;
    let bad_path = std::env::temp_dir().join(format!(
        "ae-indexed-field-store-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&bad_path, bad_source).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&bad_path)
        .output()
        .unwrap();
    fs::remove_file(&bad_path).unwrap();
    assert!(!result.status.success(), "expected a field store through an index to stay refused");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("resolved route"),
        "the refusal must stay the route diagnostic: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// ADR-0235 slice 4: `vec[vec]` is the first owned element type. The outer
/// vector owns each inner header; `push`, `pop`, overwrite and scope exit
/// release through the runtime's drop hook, element reads are views, and the
/// compiled, MIR, tree and interpreter backends agree on every operation.
#[test]
fn nested_vector_elements_drop_and_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("nested_vectors.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "moved inner vector",
            "fn main() { var rows = vec[vec[1]]; var row = vec[2]; push(rows, row); print(len(row)); }",
            "cannot use 'row' because it was moved",
        ),
        (
            "borrowed element binding",
            "fn main() { var rows = vec[vec[1]]; let row = rows[0]; print(len(row)); }",
            "cannot store a borrowed vector in a variable",
        ),
        (
            "mixed inner literals",
            "fn main() { let rows = vec[vec[1], vec[2.5]]; }",
            "vector elements must all be",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-nested-vectors-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// ADR-0235 slice 5: an owned struct element (`vec[Tagged]`) stores the
/// struct by value and releases each element exactly once on pop, overwrite
/// and scope exit through the drop hook. Element reads are `&Struct` views:
/// fields read inline, a `for` variable forwards to `&Struct` parameters, and
/// the element cannot be bound or moved out. The interpreter runs the same
/// drops in the same order as the compiled backends.
#[test]
fn owned_struct_vector_elements_drop_and_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("owned_struct_vector.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "element view pushed into its own vector",
            r#"struct Tagged { name: string, n: i64 } fn main() { var items = vec[Tagged { name: "a", n: 1 }]; push(items, items[0]); }"#,
            "vector elements must be",
        ),
        (
            "borrowed element binding",
            r#"struct Tagged { name: string, n: i64 } fn main() { var items = vec[Tagged { name: "a", n: 1 }]; let t = items[0]; print(t.n); }"#,
            "cannot store a borrowed struct in a variable",
        ),
        (
            "field moved into the vector",
            r#"struct Tagged { name: string, n: i64 } struct Holder { item: Tagged } fn main() { var items = vec[Tagged { name: "a", n: 1 }]; let h = Holder { item: Tagged { name: "b", n: 2 } }; push(items, h.item); }"#,
            "cannot move a field out of a struct",
        ),
        (
            "stored value used after the move",
            r#"struct Tagged { name: string, n: i64 } fn main() { var items = vec[Tagged { name: "a", n: 1 }]; var extra = Tagged { name: "c", n: 3 }; items[0] = extra; print(extra.n); }"#,
            "cannot use 'extra' because it was moved",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-owned-struct-vectors-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// ADR-0235 slice 6: string vector literals. `vec["a", "b"]` builds a
/// `vec[string]` where text literals are owned, a string view (field read,
/// `slice_view`, string-vector element) is copied in, and an owned string
/// variable moves in — the vector never aliases the struct that still owns a
/// field. `push(v, p.name)` follows the same copy rule (a pre-existing
/// aliasing bug: the field bytes were handed to the vector and both owners
/// freed them).
#[test]
fn string_vector_literals_copy_views_and_move_owned_strings() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("string_vector_literals.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "moved string element used after the literal",
            r#"fn main() { var s = "delta"; var w = vec[s]; print(s); }"#,
            "cannot use 's' because it was moved",
        ),
        (
            "mixed string and integer elements",
            r#"fn main() { let v = vec["a", 1]; }"#,
            "vector elements must all be string",
        ),
        (
            "field moved out instead of copied",
            r#"struct L { name: string, n: i64 } fn main() { let p = L { name: "a", n: 1 }; let s = p.name; print(s); }"#,
            "cannot move a field out of a struct",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-string-vector-literals-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: element reads through a struct-field receiver (`r.cells[0]`).
/// The receiver is an lvalue, so every vector family reads through it —
/// i64, f64, string (a view), and an owned struct element's fields — and a
/// string/owned view keeps the struct variable borrowed while a binding can
/// still read it.
#[test]
fn field_receiver_indexing_matches_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("field_receiver_indexing.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "indexing a non-vector field",
            r#"struct P { n: i64 } fn main() { let p = P { n: 1 }; print(p.n[0]); }"#,
            "cannot index a value of type i64",
        ),
        (
            "struct reassigned while a field-element view is live",
            r#"struct W { words: vec[string] } fn main() { var w = W { words: vec["alpha"] }; let s = w.words[0]; w = W { words: vec["beta"] }; print(s); }"#,
            "cannot assign to 'w' while it is borrowed",
        ),
        (
            "owned struct element bound to a local",
            r#"struct I { name: string, n: i64 } struct B { items: vec[I] } fn main() { let b = B { items: vec[I { name: "a", n: 1 }] }; let t = b.items[0]; print(t.n); }"#,
            "cannot store a borrowed struct in a variable",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-field-receiver-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: `&vec[i]` borrow arguments. The element is addressed in place and
/// passed to a `&T`/`&vec` parameter for the duration of the call; owned
/// struct elements, Copy struct elements and nested vector elements all
/// work, and the vector stays usable once the call returns.
#[test]
fn indexed_borrow_arguments_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("indexed_borrow_arguments.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "mutable element borrow",
            r#"struct T { n: i64 } fn show(t: &T) { print(t.n); } fn main() { var items = vec[T { n: 1 }]; show(&mut items[0]); }"#,
            "a place borrow is read-only",
        ),
        (
            "element shape mismatch",
            r#"fn total(row: &vec) -> i64 { return len(row); } fn main() { let words = vec["a"]; print(total(&words[0])); }"#,
            "argument must be &vec[i64]",
        ),
        (
            "element borrow after the vector moved",
            r#"struct T { n: i64 } fn show(t: &T) { print(t.n); } fn main() { var items = vec[T { n: 1 }]; let moved = items; show(&items[0]); }"#,
            "cannot borrow 'items' because it was moved",
        ),
        (
            "element borrow of a non-vector binding",
            r#"struct T { n: i64 } fn show(t: &T) { print(t.n); } fn main() { let s = "x"; show(&s[0]); }"#,
            "'&s...' needs a vector place to index",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-indexed-borrows-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: vector operations on a struct's vector field. `push`, `pop`,
/// `sort`, `reverse` and `r.cells[i] = x` address the field lvalue for every
/// family; a `&mut` root writes through its pointer; an owned element
/// releases the replaced/popped value exactly once, in order.
#[test]
fn field_vector_writes_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("field_vector_writes.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "immutable root",
            r#"struct B { items: vec } fn main() { let b = B { items: vec[1] }; push(b.items, 2); }"#,
            "cannot modify immutable variable 'b'",
        ),
        (
            "field write while an element view is live",
            r#"struct W { words: vec[string] } fn main() { var w = W { words: vec["a"] }; let s = w.words[0]; push(w.words, "b"); print(s); }"#,
            "cannot modify 'w' while it is borrowed",
        ),
        (
            "moved root",
            r#"struct B { items: vec } fn main() { var b = B { items: vec[1] }; let moved = b; push(b.items, 2); }"#,
            "cannot use 'b' because it was moved",
        ),
        (
            "field is not a vector",
            r#"struct P { n: i64 } fn main() { var p = P { n: 1 }; push(p.n, 2); }"#,
            "push expects a vec, found i64",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-field-vector-writes-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: place borrow arguments (`&h.item`, `&r.cells`, `&t.rows[0]`,
/// `&o.inner`). The place is addressed in place and the borrow lives only for
/// the call, so a struct field, a vector field and an element of a vector
/// field all reach `&T`/`&vec` parameters without moving or copying.
#[test]
fn place_borrow_arguments_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("place_borrow_arguments.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "mutable place borrow",
            r#"struct T { n: i64 } struct H { item: T } fn mutate(t: &mut T) { t.n = 2; } fn main() { var h = H { item: T { n: 1 } }; mutate(&mut h.item); }"#,
            "a place borrow is read-only",
        ),
        (
            "root moved before the borrow",
            r#"struct T { n: i64 } fn show(t: &T) { print(t.n); } fn main() { var h = T { n: 1 }; let moved = h; show(&h); }"#,
            "cannot borrow 'h' because it was moved",
        ),
        (
            "missing field",
            r#"struct T { n: i64 } struct H { item: T } fn show(t: &T) { print(t.n); } fn main() { let h = H { item: T { n: 1 } }; show(&h.missing); }"#,
            "struct 'H' has no field 'missing'",
        ),
        (
            "field on a non-struct",
            r#"struct T { n: i64 } fn show(t: &T) { print(t.n); } fn main() { let s = "abc"; show(&s.name); }"#,
            "only a struct has fields, found string",
        ),
        (
            "scalar field to a struct borrow",
            r#"struct T { n: i64 } struct H { item: T, m: i64 } fn show(t: &T) { print(t.n); } fn main() { let h = H { item: T { n: 1 }, m: 2 }; show(&h.m); }"#,
            "argument must be &struct #",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-place-borrows-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: vector operations through a nested struct-field receiver
/// (`a.b.c.cells`). The field path resolves step by step from the root
/// binding; a `&mut` root writes through its pointer at every level.
#[test]
fn nested_field_receiver_writes_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("nested_field_receiver_writes.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "moved root",
            r#"struct C { cells: vec } struct B { c: C } struct A { b: B } fn main() { var a = A { b: B { c: C { cells: vec[1] } } }; let moved = a; push(a.b.c.cells, 2); }"#,
            "cannot use 'a' because it was moved",
        ),
        (
            "non-vector final field",
            r#"struct C { n: i64 } struct B { c: C } struct A { b: B } fn main() { var a = A { b: B { c: C { n: 1 } } }; push(a.b.c.n, 2); }"#,
            "push expects a vec, found i64",
        ),
        (
            "missing intermediate field",
            r#"struct C { cells: vec } struct B { c: C } struct A { b: B } fn main() { var a = A { b: B { c: C { cells: vec[1] } }, }; push(a.b.missing.cells, 2); }"#,
            "struct 'B' has no field 'missing'",
        ),
        (
            "nested map field store",
            r#"struct M { counts: map[string, i64] } struct A { m: M } fn main() { var a = A { m: M { counts: map_new() } }; a.m.counts["k"] = 2; }"#,
            "a nested map field store is not supported yet",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-nested-field-writes-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// P2.4: field stores through a nested receiver. `o.m.inner.n = 5`,
/// `o.m.inner.name = "beta"`, `o.m.inner.cells = vec[7, 8]` and a `&mut`
/// helper's compound write all resolve the chain from the root binding; a
/// write into a moved value is rejected like any other use of it.
#[test]
fn nested_field_stores_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("nested_field_stores.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "immutable root",
            r#"struct I { n: i64 } struct H { i: I } fn main() { let h = H { i: I { n: 1 } }; h.i.n = 2; }"#,
            "cannot assign to immutable variable 'h'",
        ),
        (
            "read-only struct borrow root",
            r#"struct I { n: i64 } struct H { i: I } fn write(h: &H) { h.i.n = 2; } fn main() {}"#,
            "cannot assign to immutable variable 'h'",
        ),
        (
            "moved root",
            r#"struct I { n: i64 } struct H { i: I } fn main() { var h = H { i: I { n: 1 } }; let moved = h; h.i.n = 2; }"#,
            "cannot use 'h' because it was moved",
        ),
        (
            "one-level field write after a move",
            r#"struct P { x: i64 } fn main() { var p = P { x: 1 }; let q = p; p.x = 2; }"#,
            "cannot use 'p' because it was moved",
        ),
        (
            "missing intermediate field",
            r#"struct I { n: i64 } struct H { i: I } fn main() { var h = H { i: I { n: 1 } }; h.missing.n = 2; }"#,
            "struct 'H' has no field 'missing'",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-nested-field-stores-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// `bytes(s)` copies a string's UTF-8 bytes into an i64 vector. The
/// interpreter now implements it too, and with its `bytes` arm the interpreter
/// covers every expression variant — there is no "unsupported expression"
/// fallback left, so a new variant must add an arm (the build fails
/// otherwise) instead of silently trapping.
#[test]
fn bytes_values_match_across_backends_and_the_interpreter_is_complete() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("bytes_values.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }
}

/// P2.5.1 (M1.2.06): local type annotations. `let v: vec[f64] = vec[];`
/// names an empty literal's element type, `let n: i64 = 41;` declares a
/// scalar binding, and a mismatch reports "the annotation says X, but the
/// initializer is Y". A borrow annotation is rejected outright.
#[test]
fn typed_bindings_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("typed_bindings.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "scalar annotation mismatch",
            r#"fn main() { let n: i64 = 1.5; print(n); }"#,
            "the annotation says i64, but the initializer is f64",
        ),
        (
            "vector element annotation mismatch",
            r#"fn main() { let v: vec[f64] = vec[1, 2]; }"#,
            "the annotation says vec[f64], but the initializer is vec[i64]",
        ),
        (
            "borrow annotation",
            r#"struct P { x: i64 } fn main() { let p = P { x: 1 }; let r: &P = p; print(r.x); }"#,
            "a local binding cannot hold a borrow",
        ),
        (
            "unknown annotated type",
            r#"fn main() { let extra: Vec[i64] = vec[1]; }"#,
            "unknown type 'Vec'",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-typed-bindings-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// ADR-0236: `&[T]` slices are read-only fat pointers built at call sites
/// from a whole vector, a whole array, or another slice. Length, indexing and
/// `for` iteration read through the pointer; owned elements read as views.
#[test]
fn slices_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("slices.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source, needle) in [
        (
            "element mismatch",
            r#"fn sum(values: &[i64]) -> i64 { return len(values); } fn main() { var words = vec["a"]; print(sum(&words)); }"#,
            "argument must be &[i64], found vec[string]",
        ),
        (
            "element slice",
            r#"fn sum(values: &[i64]) -> i64 { return len(values); } fn main() { var v = vec[1, 2]; print(sum(&v[0])); }"#,
            "a slice argument borrows a whole vector or array",
        ),
        (
            "push through a slice",
            r#"fn grow(values: &[i64]) { push(values, 1); } fn main() {}"#,
            "push expects a vec, found &[i64]",
        ),
        (
            "returning a slice",
            r#"fn take(values: &[i64]) -> &[i64] { return values; } fn main() {}"#,
            "a function cannot return a borrow",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-slices-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}


/// ADR-0236 rung 3: `&v[start..end]` slices a window out of a vector, an
/// array or another slice. The bounds check is a runtime trap with the same
/// message and position on the compiled and interpreted paths.
#[test]
fn slice_ranges_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("slice_ranges.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    for (label, source) in [
        ("start past end", "fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&v[3..1])); }"),
        ("end past len", "fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&v[0..9])); }"),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-slice-range-trap-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
        let interpreted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--interp").output().unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!compiled.status.success() && !interpreted.status.success(), "{label}: both modes must trap");
        // The compiled path reaches the trap through C stdio (CRLF on
        // Windows); the interpreter writes LF. Compare the text, not the
        // platform's line-file terminator.
        let compiled_stderr = String::from_utf8_lossy(&compiled.stderr).replace("\r\n", "\n");
        let interpreted_stderr = String::from_utf8_lossy(&interpreted.stderr).replace("\r\n", "\n");
        assert_eq!(
            compiled_stderr,
            interpreted_stderr,
            "{label}: trap position/message differ"
        );
        assert!(
            String::from_utf8_lossy(&compiled.stderr).contains("slice range out of bounds"),
            "{label}: got {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
    }

    for (label, source, needle) in [
        (
            "open end",
            r#"fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&v[1..])); }"#,
            "a slice range needs both bounds",
        ),
        (
            "non-integer bound",
            r#"fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&v["a"..2])); }"#,
            "slice range bounds must be integers",
        ),
        (
            "field range",
            r#"struct R { cells: vec } fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var r = R { cells: vec[1, 2] }; print(take(&r.cells[0..1])); }"#,
            "field slices are not supported yet",
        ),
        (
            "element slice",
            r#"fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&v[0])); }"#,
            "element slices are not supported yet",
        ),
        (
            "mutable range",
            r#"fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; print(take(&mut v[0..1])); }"#,
            "this parameter borrows read-only; write the argument as '&name'",
        ),
        (
            "moved source",
            r#"fn take(s: &[i64]) -> i64 { return len(s); } fn main() { var v = vec[1, 2]; let w = v; print(len(w)); print(take(&v[0..1])); }"#,
            "cannot borrow 'v' because it was moved",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-slice-range-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}


/// ADR-0236 composes with generics: `&[T]` parameters instantiate per
/// element type (`size[i64]`, `size[string]`), and len / for / sub-windows
/// work on each instantiation. Calls spell their type arguments until
/// P2.5.2 inference lands.
#[test]
fn generic_slices_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("generic_slices.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }
    // P2.5.2: a bare generic slice call infers `&[T]` from `&a`, while a
    // genuinely uninferable parameter (such as an extra type parameter with no
    // inferable argument) still requires explicit type arguments.
    let path = std::env::temp_dir().join(format!(
        "ae-generic-slices-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(
        &path,
        "fn size[T](v: &[T]) -> i64 { return len(v); } fn main() { let a = vec[1, 2]; print(size(&a)); }",
    )
    .unwrap();
    let inferred = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        inferred.status.success(),
        "inferred generic slice call must succeed: {}",
        String::from_utf8_lossy(&inferred.stderr)
    );
    fs::write(
        &path,
        "fn size_tagged[T, U](v: &[T]) -> i64 { return len(v); } fn main() { let a = vec[1, 2]; print(size_tagged(&a)); }",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!result.status.success(), "the uninferable call must be rejected");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("write its type arguments"),
        "expected the type-argument remedy, got {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // A generic slice instantiates to the spelled element; a source of a
    // different element is rejected with the concrete instantiated type.
    for (label, source, needle) in [
        (
            "i64 slice, string source",
            r#"fn size[T](v: &[T]) -> i64 { return len(v); } fn main() { let w = vec["a"]; print(size[i64](&w)); }"#,
            "argument must be &[i64], found vec[string]",
        ),
        (
            "string slice, i64 source",
            r#"fn size[T](v: &[T]) -> i64 { return len(v); } fn main() { let a = vec[1]; print(size[string](&a)); }"#,
            "argument must be &[string], found vec[i64]",
        ),
        (
            "vec slice, scalar source",
            r#"fn size[T](v: &[T]) -> i64 { return len(v); } fn main() { let a = vec[1]; print(size[vec](&a)); }"#,
            "argument must be &[vec[i64]], found vec[i64]",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-generic-slices-mis-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}


/// ADR-0236 rung 2: a place borrow names one element of a slice parameter
/// (`&items[0]`, `&rows[0]`) and passes it to a `&Struct`/`&vec` parameter,
/// addressed in place exactly like the vector element borrow.
#[test]
fn slice_element_borrows_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("slice_element_borrows.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    // A string element is not a place-borrow target (same as the vector form).
    let path = std::env::temp_dir().join(format!(
        "ae-slice-elem-borrow-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(
        &path,
        "fn sl(s: &string) -> i64 { return len(s); } fn f(words: &[string]) -> i64 { return sl(&words[0]); } fn main() { let w = vec[\"hello\"]; print(f(&w)); }",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!result.status.success(), "a string element borrow must be rejected");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("a borrow expression must be cast to a pointer"),
        "expected the pointer-cast remedy, got {}",
        String::from_utf8_lossy(&result.stderr)
    );
}


/// ADR-0236 rung 4: `&mut [T]` stores through the fat pointer. The owner is a
/// whole vector binding, the call site checks exclusivity, and Copy elements
/// (i64/f64/Copy struct) are writable; the four backends agree on the values.
#[test]
fn slice_mut_match_across_backends() {
    fn lines(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().collect::<Vec<_>>().join("\n")
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("slice_mut.ae");
    let expected = lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [("mir", Some("mir"), false), ("tree", Some("tree"), false), ("default", None, false), ("interp", None, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lines(&output.stdout), expected, "mode {label} output differs");
    }

    // An out-of-range store traps at the same position on both paths (the
    // container-index wording differs, as it already does for vector reads).
    let path = std::env::temp_dir().join(format!(
        "ae-slice-mut-oob-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(
        &path,
        "fn f(m: &mut [i64]) { m[5] = 1; } fn main() { var v = vec[1]; f(&mut v); }",
    )
    .unwrap();
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
    let interpreted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--interp").output().unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!compiled.status.success() && !interpreted.status.success(), "both modes must trap");
    let compiled_err = String::from_utf8_lossy(&compiled.stderr);
    let interpreted_err = String::from_utf8_lossy(&interpreted.stderr);
    assert!(compiled_err.contains("index out of bounds"), "got {compiled_err}");
    assert!(interpreted_err.contains("index out of bounds"), "got {interpreted_err}");
    assert_eq!(compiled_err.split(':').take(3).collect::<Vec<_>>(), interpreted_err.split(':').take(3).collect::<Vec<_>>(), "trap position differs");

    // A bad mutable window traps at the call site with the same text and
    // position on both paths.
    let path = std::env::temp_dir().join(format!(
        "ae-slice-mut-window-{}-{}.ae",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(
        &path,
        "fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var v = vec[1, 2]; f(&mut v[2..1]); }",
    )
    .unwrap();
    let compiled = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).output().unwrap();
    let interpreted = Command::new(env!("CARGO_BIN_EXE_ae")).arg("run").arg(&path).arg("--interp").output().unwrap();
    fs::remove_file(&path).unwrap();
    assert!(!compiled.status.success() && !interpreted.status.success(), "both modes must trap");
    assert_eq!(
        String::from_utf8_lossy(&compiled.stderr).replace("\r\n", "\n"),
        String::from_utf8_lossy(&interpreted.stderr).replace("\r\n", "\n"),
        "window trap position/message differ"
    );
    assert!(String::from_utf8_lossy(&compiled.stderr).contains("slice range out of bounds"));

    for (label, source, needle) in [
        (
            "missing &mut",
            r#"fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var v = vec[1]; f(&v); }"#,
            "this parameter borrows mutably; write the argument as '&mut name'",
        ),
        (
            "bare name",
            r#"fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var v = vec[1]; f(v); }"#,
            "a mutable slice argument is written '&mut name'",
        ),
        (
            "immutable owner",
            r#"fn f(m: &mut [i64]) { m[0] = 1; } fn main() { let v = vec[1]; f(&mut v); }"#,
            "cannot borrow 'v' mutably: it was declared without 'var'",
        ),
        (
            "mutable arg to a read-only parameter",
            r#"fn f(s: &[i64]) -> i64 { return s[0]; } fn main() { var v = vec[1]; print(f(&mut v)); }"#,
            "this parameter borrows read-only; write the argument as '&name'",
        ),
        (
            "store through a read-only slice",
            r#"fn f(s: &[i64]) { s[0] = 1; } fn main() {}"#,
            "a read-only slice cannot be written; take the parameter as '&mut [T]'",
        ),
        (
            "owned element store",
            r#"struct T { n: i64 } fn f(m: &mut [T]) { m[0] = T { n: 1 }; } fn main() {}"#,
            "storing through a mutable slice of owned elements is not supported yet",
        ),
        (
            "forwarded mutable slice",
            r#"fn g(m: &mut [i64]) { m[0] = 1; } fn f(m: &mut [i64]) { g(&mut m); } fn main() {}"#,
            "forwarding a mutable slice is not supported yet",
        ),
        (
            "mutable element borrow",
            r#"fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var v = vec[1, 2]; f(&mut v[0]); }"#,
            "a single element is not a mutable slice",
        ),
        (
            "mutable window open bound",
            r#"fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var v = vec[1, 2]; f(&mut v[1..]); }"#,
            "a slice range needs both bounds",
        ),
        (
            "field mutable slice",
            r#"struct R { cells: vec } fn f(m: &mut [i64]) { m[0] = 1; } fn main() { var r = R { cells: vec[1] }; f(&mut r.cells); }"#,
            "field slices are not supported yet",
        ),
        (
            "wrong store type",
            r#"fn f(m: &mut [i64]) { m[0] = "x"; } fn main() {}"#,
            "slice elements must be i64",
        ),
        (
            "mutable borrow inside a loop over the owner",
            r#"fn touch(m: &mut [i64]) { m[0] = 0; } fn main() { var v = vec[1, 2]; for x in v { print(x); touch(&mut v); } }"#,
            "cannot borrow 'v' mutably while it is borrowed in this statement",
        ),
        (
            "two mutable borrows in one statement",
            r#"fn pa(a: &mut [i64], b: &mut [i64]) -> i64 { a[0] = 1; b[0] = 2; return a[0]; } fn main() { var v = vec[1]; print(pa(&mut v, &mut v)); }"#,
            "cannot borrow 'v' mutably while it is borrowed in this statement",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "ae-slice-mut-bad-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!result.status.success(), "{label} must be rejected");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(needle),
            "{label}: expected '{needle}', got {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}


#[test]
fn emit_pure_c_outputs_runnable_c_and_fails_closed() {
    // The pure-MIR C pipeline (ADR-0237) is reachable from the CLI and its
    // output is a runnable translation unit, like the legacy `emit-c`.
    let dir = unique_temp_dir("emit_pure_c");
    let source = dir.join("program.ae");
    fs::write(&source, "fn main() { print(6 * 7); }").unwrap();
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-pure-c")
        .arg(&source)
        .output()
        .unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let text = String::from_utf8_lossy(&emitted.stdout);
    assert!(text.contains("ae_fn_main") && text.contains("int main"), "{text}");

    let c_path = dir.join("program.c");
    let exe_path = dir.join(if cfg!(windows) { "program.exe" } else { "program" });
    fs::write(&c_path, &emitted.stdout).unwrap();
    let cc = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let build = Command::new(cc)
        .args(["-std=c11", "-O0", &c_path.to_string_lossy(), "-o", &exe_path.to_string_lossy(), "-lm"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let run = Command::new(&exe_path).output().unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"), "42\n");

    // Shapes the pipeline does not support yet fail closed with a named
    // diagnostic instead of emitting malformed C. Externs need the backend's
    // own runtime unit (ADR-0238 stage 3), so they are still rejected.
    let unsupported = dir.join("unsupported.ae");
    fs::write(
        &unsupported,
        "extern \"C\" fn ae_std_alloc(size: i64, line: i64, column: i64) -> *mut i64;\nfn main() { unsafe { let p = ae_std_alloc(8, 1, 1); } }\n",
    )
    .unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-pure-c")
        .arg(&unsupported)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        message.contains("is not supported by the pure C backend")
            || message.contains("is not linked by the pure C backend")
            || message.contains("cannot handle"),
        "{message}"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emit_pure_c_owns_and_releases_strings() {
    // ADR-0238 stage 1: one `ae_str` value type carries a literal's static
    // bytes and an allocated buffer's ownership; concatenation consumes the
    // owned operands, a move takes the slot, and a drop releases it.
    let dir = unique_temp_dir("emit_pure_c_strings");
    let source = dir.join("program.ae");
    fs::write(
        &source,
        r#"struct Label { name: string, n: i64 }

fn tag(s: string) -> string {
    return "tag:" + s;
}

fn main() {
    var s = "a" + "b";
    print(s);
    let t = s;
    print(t);
    let u = tag(t);
    print(u);
    print(len(u));
    let v = slice(u, 0, 3);
    print(v);
    let w = slice_view(u, 4, 6);
    print(w);
    print(u == "tag:ab");
    print(u != "nope");
    let p = Label { name: "z" + to_string(7), n: 7 };
    print(p.name);
    print(p.n);
}
"#,
    )
    .unwrap();
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-pure-c")
        .arg(&source)
        .output()
        .unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let text = String::from_utf8_lossy(&emitted.stdout);
    assert!(
        text.contains("typedef struct { const char *ptr; int64_t len; bool owned; } ae_str;"),
        "{text}"
    );
    assert!(text.contains("ae_str_take(&"), "a move must leave the source slot empty: {text}");
    assert!(text.contains("ae_str_release(&"), "an owned value must be released: {text}");
    assert!(text.contains("ae_str_concat("), "{text}");

    let c_path = dir.join("program.c");
    let exe_path = dir.join(if cfg!(windows) { "program.exe" } else { "program" });
    fs::write(&c_path, &emitted.stdout).unwrap();
    let cc = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let build = Command::new(&cc)
        .args(["-std=c11", "-O0", &c_path.to_string_lossy(), "-o", &exe_path.to_string_lossy(), "-lm"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let run = Command::new(&exe_path).output().unwrap();
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"),
        "ab\nab\ntag:ab\n6\ntag\nab\ntrue\ntrue\nz7\n7\n"
    );

    // Every allocation above is released exactly once. Linux CI runs the
    // same binary under ASan+UBSan; MinGW has no sanitizers, so the plain
    // run is the Windows evidence.
    #[cfg(target_os = "linux")]
    {
        let asan_path = dir.join("program_asan");
        let build = Command::new(&cc)
            .args([
                "-std=c11",
                "-O0",
                "-g",
                "-fsanitize=address,undefined",
                &c_path.to_string_lossy(),
                "-o",
                &asan_path.to_string_lossy(),
                "-lm",
            ])
            .output()
            .unwrap();
        assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
        let run = Command::new(&asan_path).output().unwrap();
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(run.status.success(), "{stderr}");
        assert!(
            !stderr.contains("LeakSanitizer") && !stderr.contains("AddressSanitizer"),
            "{stderr}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

// Appended test: vectors (ADR-0238 stage 2) grow, store, iterate, nest and
// release their element storage.
#[test]
fn emit_pure_c_owns_and_releases_vectors() {
    let dir = unique_temp_dir("emit_pure_c_vectors");
    let source = dir.join("program.ae");
    fs::write(
        &source,
        r#"fn main() {
    var v = vec[1, 2];
    push(v, 3);
    print(len(v));
    print(v[0]);
    v[0] = 9;
    print(v[0]);
    pop(v);
    print(len(v));

    var words = vec["a", "b"];
    push(words, "c");
    for w in words {
        print(w);
    }

    var rows = vec[vec[1, 2], vec[3]];
    let p = vec[4, 4];
    push(rows, p);
    print(len(rows));
    print(len(rows[2]));
}
"#,
    )
    .unwrap();
    let emitted = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("emit-pure-c")
        .arg(&source)
        .output()
        .unwrap();
    assert!(emitted.status.success(), "{}", String::from_utf8_lossy(&emitted.stderr));
    let text = String::from_utf8_lossy(&emitted.stdout);
    assert!(text.contains("typedef struct {\n    void *data;\n    int64_t len;\n    int64_t cap;\n    int64_t elem_size;\n    void (*drop)(void *);\n} ae_vec;"), "{text}");
    assert!(text.contains("ae_vec_push("), "{text}");
    assert!(text.contains("ae_vec_at("), "{text}");
    assert!(text.contains("ae_vec_release(&"), "{text}");

    let c_path = dir.join("program.c");
    let exe_path = dir.join(if cfg!(windows) { "program.exe" } else { "program" });
    fs::write(&c_path, &emitted.stdout).unwrap();
    let cc = std::env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let build = Command::new(&cc)
        .args(["-std=c11", "-O0", &c_path.to_string_lossy(), "-o", &exe_path.to_string_lossy(), "-lm"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let run = Command::new(&exe_path).output().unwrap();
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"),
        "3\n1\n9\n2\na\nb\nc\n3\n2\n"
    );

    #[cfg(target_os = "linux")]
    {
        let asan_path = dir.join("program_asan");
        let build = Command::new(&cc)
            .args([
                "-std=c11",
                "-O0",
                "-g",
                "-fsanitize=address,undefined",
                &c_path.to_string_lossy(),
                "-o",
                &asan_path.to_string_lossy(),
                "-lm",
            ])
            .output()
            .unwrap();
        assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
        let run = Command::new(&asan_path).output().unwrap();
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(run.status.success(), "{stderr}");
        assert!(
            !stderr.contains("LeakSanitizer") && !stderr.contains("AddressSanitizer"),
            "{stderr}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}