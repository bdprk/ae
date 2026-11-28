use std::fs;
use std::path::Path;

use ae_compiler::ae_mir::{StatementKind, TerminatorKind, format_body, validate};
use ae_compiler::{format_pure_mir_path, lower_path_to_pure_mir};

fn repo_path(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

#[test]
fn lowers_examples_hello_and_option_propagate() {
    for rel in ["examples/hello.ae", "examples/option_propagate.ae"] {
        let path = repo_path(rel);
        let bodies = lower_path_to_pure_mir(&path)
            .unwrap_or_else(|e| panic!("failed to lower {rel}: {e}"));
        assert!(!bodies.is_empty(), "{rel} should produce at least one MIR body");
        for body in &bodies {
            validate(body).unwrap_or_else(|errs| {
                panic!("{rel} body `{}` failed validation: {errs:?}", body.name)
            });
        }
        let fmt1 = format_pure_mir_path(&path).expect("format pass 1");
        let fmt2 = format_pure_mir_path(&path).expect("format pass 2");
        assert_eq!(fmt1, fmt2, "{rel} MIR formatting must be deterministic");
    }
}

#[test]
fn lowers_short_circuit_and_or_to_switch_int_cfg() {
    let dir = std::env::temp_dir().join("ae_p31_short_circuit_test");
    let _ = fs::create_dir_all(&dir);
    let file = dir.join("short_circuit.ae");
    fs::write(
        &file,
        r#"
fn check_and(a: bool, b: bool) -> bool {
    let res = a && b;
    return res;
}

fn check_or(a: bool, b: bool) -> bool {
    let res = a || b;
    return res;
}

fn check_branch(a: bool, b: bool, c: bool) -> i64 {
    if (a && b) || c {
        return 1;
    }
    return 0;
}

fn main() {
    let x = check_and(true, false);
    let y = check_or(false, true);
    let z = check_branch(x, y, true);
    print(z);
}
"#,
    )
    .expect("write temp source");

    let bodies = lower_path_to_pure_mir(&file).expect("lower short-circuit program");
    assert_eq!(bodies.len(), 4);
    for body in &bodies {
        validate(body).unwrap_or_else(|errs| {
            panic!("body `{}` failed validation: {errs:?}", body.name)
        });
    }

    // Verify check_and, check_or, and check_branch each lowered into multi-block SwitchInt CFGs.
    for body in &bodies[..3] {
        assert!(
            body.basic_blocks.len() > 1,
            "short-circuit function `{}` must lower to multiple basic blocks, got:\n{}",
            body.name,
            format_body(body)
        );
        let switch_count = body
            .basic_blocks
            .iter()
            .filter(|bb| {
                matches!(
                    bb.terminator.as_ref().map(|t| &t.kind),
                    Some(TerminatorKind::SwitchInt { .. })
                )
            })
            .count();
        assert!(
            switch_count >= 1,
            "short-circuit function `{}` must use SwitchInt, got:\n{}",
            body.name,
            format_body(body)
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn lowers_loops_nested_matches_generics_structs_and_drops() {
    let dir = std::env::temp_dir().join("ae_p31_features_test");
    let _ = fs::create_dir_all(&dir);
    let file = dir.join("features.ae");
    fs::write(
        &file,
        r#"
struct Pair {
    first: i64,
    second: string,
}

enum Inner {
    Val(i64),
    Empty,
}

enum Outer {
    Wrap(Inner),
    None,
}

fn identity[T](x: T) -> T {
    return x;
}

fn sum_loop(n: i64) -> i64 {
    var acc = 0;
    var i = 0;
    while i < n {
        acc = acc + i;
        i = i + 1;
    }
    for j in 0..n {
        acc = acc + j;
    }
    return acc;
}

fn unwrap_nested(o: Outer) -> i64 {
    match o {
        Outer::Wrap(Inner::Val(v)) => {
            return v;
        }
        Outer::Wrap(Inner::Empty) => {
            return 0;
        }
        Outer::None => {
            return -1;
        }
    }
}

fn make_and_drop() -> i64 {
    let p = Pair { first: 42, second: "hello" };
    let v = identity[i64](p.first);
    return v;
}

fn main() {
    let s = sum_loop(5);
    let n = unwrap_nested(Outer::Wrap(Inner::Val(s)));
    let d = make_and_drop();
    print(n + d);
}
"#,
    )
    .expect("write temp features source");

    let bodies = lower_path_to_pure_mir(&file).expect("lower features program");
    assert!(bodies.len() >= 5);
    for body in &bodies {
        validate(body).unwrap_or_else(|errs| {
            panic!("body `{}` failed validation: {errs:?}", body.name)
        });
    }

    // Verify at least one body emits explicit Drop statements for owned locals.
    let has_drop = bodies.iter().any(|b| {
        b.basic_blocks.iter().any(|bb| {
            bb.statements
                .iter()
                .any(|s| matches!(s.kind, StatementKind::Drop(_)))
        })
    });
    assert!(has_drop, "expected explicit Drop statements for owned struct/string locals");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn lowers_run_corpus_fixtures_to_valid_pure_mir() {
    let run_dir = repo_path("tests/run");
    let mut paths: Vec<std::path::PathBuf> = fs::read_dir(&run_dir)
        .expect("read tests/run")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "ae"))
        .collect();
    paths.sort();
    assert!(paths.len() > 50, "expected run corpus, found {}", paths.len());

let mut lowered_count = 0usize;
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for path in &paths {
        // Only test standalone programs that check cleanly with default imports.
        if ae_compiler::frontend::check_path(path).is_err() {
            continue;
        }
        match lower_path_to_pure_mir(path) {
            Ok(bodies) => {
                for body in &bodies {
                    if let Err(errs) = validate(body) {
                        failures.push(format!(
                            "{} ({}): validation error: {errs:?}",
                            path.display(),
                            body.name
                        ));
                    }
                }
                lowered_count += 1;
            }
            // The pipeline fails closed on shapes it cannot lower yet; those
            // are tracked skips, not failures (the message is the contract).
            Err(err) if err.contains("pure MIR lowering cannot handle") => {
                skipped.push(format!("{}: {err}", path.display()));
            }
            Err(err) => {
                failures.push(format!("{}: {err}", path.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "pure MIR lowering/validation failures:\n{}",
        failures.join("\n")
    );
    assert!(lowered_count > 50, "expected >50 lowered fixtures, got {lowered_count}");
    // The known-unsupported set is small and documented; a surprise growth is
    // a regression, not a silent pass.
    assert!(
        skipped.len() <= 5,
        "unsupported-construct set grew to {} fixtures:\n{}",
        skipped.len(),
        skipped.join("\n")
    );
}

#[test]
fn invalid_program_returns_err_without_panicking() {
    let dir = std::env::temp_dir().join("ae_p31_invalid_test");
    let _ = fs::create_dir_all(&dir);
    let file = dir.join("invalid.ae");
    fs::write(&file, "fn main() { let x: i64 = \"not an int\"; }\n").expect("write invalid source");
    let res = lower_path_to_pure_mir(&file);
    assert!(res.is_err(), "invalid program should return Err");
    let _ = fs::remove_dir_all(&dir);
}
