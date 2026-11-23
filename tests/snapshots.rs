//! Snapshot tests (plan M0.6/M0.7).
//!
//! * `tests/ui/*.ae`   — `ae check`; expected stderr in `*.stderr`
//!   (empty or missing file means the program must be accepted).
//! * `tests/ui-json/*.ae` — `ae check --json`; expected output in `*.json`.
//! * `tests/run/*.ae`  — `ae run`; expected stdout in `*.stdout`,
//!   optional stderr in `*.stderr` and exit code in `*.exit` (default 0).
//!
//! Set `AE_BLESS=1` to (re)write output expectations. Exit expectations
//! are never blessed: write a nonzero `.exit` explicitly for a failing case.
//! Output is compared after CRLF -> LF normalisation. Diagnostic paths within
//! the absolute test directory are replaced by `$DIR` for portability.
use std::{env, fs, path::{Path, PathBuf}, process::Command};

fn bless() -> bool { env::var_os("AE_BLESS").is_some_and(|v| v == "1") }

fn cases(dir: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(dir);
    let mut out: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .map(|e| e.expect("cannot read test directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "ae"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no snapshot cases in {}", root.display());
    out
}

fn normalise(text: &[u8], dir: &Path) -> String {
    let s = String::from_utf8_lossy(text).replace("\r\n", "\n");
    let d = dir.display().to_string();
    s.replace(&format!("{}\\\\", d.replace('\\', "\\\\")), "$DIR/")
        .replace(&format!("{d}\\"), "$DIR/")
        .replace(&format!("{}/", d.replace('\\', "/")), "$DIR/")
        .replace(&d, "$DIR")
}

fn compare(path: &Path, ext: &str, actual: &str, failures: &mut Vec<String>, update: bool) {
    let expect_path = path.with_extension(ext);
    if update {
        if actual.is_empty() && ext == "stderr" { let _ = fs::remove_file(&expect_path); } else { fs::write(&expect_path, actual).unwrap(); }
        return;
    }
    let expected = match fs::read_to_string(&expect_path) {
        Ok(value) => value.replace("\r\n", "\n"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ext == "stderr" => String::new(),
        Err(e) => { failures.push(format!("{}: cannot read expectation: {e}", expect_path.display())); return; }
    };
    if expected != actual {
        failures.push(format!("{}: {ext} mismatch\n--- expected\n{expected}\n--- actual\n{actual}", path.display()));
    }
}

#[test]
fn ui_snapshots() {
    let mut failures = Vec::new();
    for case in cases("ui") {
        let dir = case.parent().unwrap().to_path_buf();
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg(&case).output().unwrap();
        if out.status.code().is_none() || out.status.success() != out.stderr.is_empty() {
            failures.push(format!("{}: inconsistent check status {} and stderr", case.display(), out.status));
            continue;
        }
        compare(&case, "stderr", &normalise(&out.stderr, &dir), &mut failures, bless());
        let expects_error = fs::read(case.with_extension("stderr")).is_ok_and(|s| !s.is_empty());
        if !bless() && (out.status.success() == expects_error || out.status.code().is_none()) {
            failures.push(format!("{}: unexpected check status {}", case.display(), out.status));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn json_ui_snapshots() {
    let mut failures = Vec::new();
    for case in cases("ui-json") {
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("check").arg("--json").arg(&case).output().unwrap();
        if out.status.code().is_none() || !out.stderr.is_empty() || out.status.success() {
            failures.push(format!("{}: expected a diagnostic and exit failure, got {}: {}", case.display(), out.status, String::from_utf8_lossy(&out.stderr)));
            continue;
        }
        if String::from_utf8_lossy(&out.stdout).matches("\"message\":").count() < 2 {
            failures.push(format!("{}: expected multiple diagnostics, got {}", case.display(), String::from_utf8_lossy(&out.stdout)));
            continue;
        }
        compare(&case, "json", &normalise(&out.stdout, case.parent().unwrap()), &mut failures, bless());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn run_snapshots() {
    let mut failures = Vec::new();
    for case in cases("run") {
        let exit_path = case.with_extension("exit");
        let expected: i32 = match fs::read_to_string(&exit_path) {
            Ok(s) => s.trim().parse().unwrap_or_else(|e| panic!("{}: invalid exit code: {e}", exit_path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => panic!("{}: {e}", exit_path.display()),
        };
        let mut outputs = Vec::new();
        let mut valid_statuses = true;
        for release in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_ae"));
            cmd.arg("run").arg(&case);
            if release { cmd.arg("--release"); }
            let out = cmd.output().unwrap();
            if out.status.code() != Some(expected) {
                failures.push(format!("{} (release={release}): status {}, expected exit {expected}\nstderr: {}", case.display(), out.status, String::from_utf8_lossy(&out.stderr)));
                valid_statuses = false;
            }
            outputs.push((release, out));
        }
        if !valid_statuses { continue; }
        for (release, out) in outputs {
            compare(&case, "stdout", &String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"), &mut failures, bless() && !release);
            compare(&case, "stderr", &normalise(&out.stderr, case.parent().unwrap()), &mut failures, bless() && !release);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `tests/types/*.ae` — `ae emit-types`; expected dump in `*.types`. The dump
/// is the resolver's type facts (signatures, item spellings, local shapes and
/// every checked/deferred/rejected expression), so it is the P1.2 baseline a
/// later unified interner must keep byte-identical.
#[test]
fn types_snapshots() {
    let mut failures = Vec::new();
    for case in cases("types") {
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-types").arg(&case).output().unwrap();
        if !out.status.success() {
            failures.push(format!("{}: emit-types failed: {}", case.display(), String::from_utf8_lossy(&out.stderr)));
            continue;
        }
        if !out.stderr.is_empty() {
            failures.push(format!("{}: emit-types wrote stderr: {}", case.display(), String::from_utf8_lossy(&out.stderr)));
        }
        compare(&case, "types", &normalise(&out.stdout, case.parent().unwrap()), &mut failures, bless());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `tests/resolve/*.ae` — `ae emit-resolve`; expected dump in `*.resolve`.
/// The resolver is its own pipeline stage (ADR-0224 R3b), so unlike the ui
/// family a case may carry diagnostics inside its dump: the comparison is
/// stdout only, and stderr must stay empty.
#[test]
fn resolve_snapshots() {
    let mut failures = Vec::new();
    for case in cases("resolve") {
        let out = Command::new(env!("CARGO_BIN_EXE_ae")).arg("emit-resolve").arg(&case).output().unwrap();
        if !out.status.success() {
            failures.push(format!("{}: emit-resolve failed: {}", case.display(), String::from_utf8_lossy(&out.stderr)));
            continue;
        }
        if !out.stderr.is_empty() {
            failures.push(format!("{}: emit-resolve wrote stderr: {}", case.display(), String::from_utf8_lossy(&out.stderr)));
        }
        compare(&case, "resolve", &normalise(&out.stdout, case.parent().unwrap()), &mut failures, bless());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
