//! P3.2 end-to-end gate: the pure-MIR -> C11 pipeline must emit, compile and
//! run every accepted `tests/run` fixture, and the observable behavior must
//! match byte-for-byte: stdout for a completed program, and stdout + the
//! positioned stderr line + the exit status for a trap fixture.
//!
//! The floor is 164 of 164 accepted fixtures: the corpus is complete at this
//! slice, so any drop is a regression. Emission failures are grouped by
//! reason in the assertion message so the remaining gaps stay visible.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ae_compiler::ae_codegen_c::EmitConfig;
use ae_compiler::ae_layout::TargetSpec;
use ae_compiler::compile_path_pure_mir_to_c;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fixtures the pipeline can emit, compile and run with matching output at
/// this slice. Raise when coverage grows; never lower without a ledger entry.
const PASS_FLOOR: usize = 164;

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn normalize(message: &str) -> String {
    let mut out = String::new();
    let mut last_digit = false;
    for ch in message.chars() {
        if ch.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            last_digit = false;
            out.push(ch);
        }
    }
    out
}

#[test]
fn pure_mir_c11_pipeline_matches_fixture_output() {
    let mut paths: Vec<PathBuf> = fs::read_dir(repo_path("tests/run"))
        .expect("read tests/run")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ae"))
        .collect();
    paths.sort();

    let config = EmitConfig { target: TargetSpec::host(), emit_main_wrapper: true };
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".to_owned());
    let dir = std::env::temp_dir().join(format!("ae_pure_corpus_gate_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");

    let mut accepted = 0usize;
    let mut passed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut reasons: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut note = |kind: &str, message: String| {
        let key = format!("{kind}: {}", normalize(&message));
        let entry = reasons.entry(key).or_insert((0, message));
        entry.0 += 1;
    };

    for path in &paths {
        if ae_compiler::frontend::check_path(path).is_err() {
            continue;
        }
        accepted += 1;
        let label = path.strip_prefix(repo_path("")).unwrap_or(path).display().to_string();
        let c_source = match compile_path_pure_mir_to_c(path, &config) {
            Ok(source) => source,
            Err(err) => {
                note("EMIT", err);
                continue;
            }
        };
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let c_path = dir.join(format!("p{id}.c"));
        let exe_path = dir.join(if cfg!(windows) { format!("p{id}.exe") } else { format!("p{id}") });
        fs::write(&c_path, &c_source).expect("write generated C");
        let build = Command::new(&cc)
            .args(["-std=c11", "-O0", &c_path.to_string_lossy(), "-o", &exe_path.to_string_lossy(), "-lm"])
            .output()
            .expect("invoke host C compiler");
        if !build.status.success() {
            let first = String::from_utf8_lossy(&build.stderr)
                .lines()
                .find(|line| line.contains("error:"))
                .unwrap_or("C compile failed")
                .to_owned();
            note("CC", format!("{label}: {first}"));
            continue;
        }
        let run = Command::new(&exe_path).output().expect("run generated binary");
        let expected = fs::read_to_string(path.with_extension("stdout")).unwrap_or_default();
        let expected = expected.replace("\r\n", "\n");
        let actual = String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n");
        let stdout_matches = actual.trim_end() == expected.trim_end();

        if !run.status.success() {
            // An expected trap: the language makes the exit status and the
            // positioned stderr line observable, so both join the comparison.
            let expected_err = fs::read_to_string(path.with_extension("stderr"))
                .unwrap_or_default()
                .replace("\r\n", "\n");
            let expected_exit = fs::read_to_string(path.with_extension("exit"))
                .unwrap_or_default()
                .trim()
                .parse::<i32>()
                .unwrap_or(1);
            let actual_err = String::from_utf8_lossy(&run.stderr).replace("\r\n", "\n");
            let code = run.status.code().unwrap_or(-1);
            if !expected_err.is_empty()
                && code == expected_exit
                && actual_err.trim_end() == expected_err.trim_end()
                && stdout_matches
            {
                passed += 1;
            } else {
                note(
                    "RUN",
                    format!(
                        "{label}: exit {code} (want {expected_exit}), stderr {:?} (want {:?})",
                        actual_err.trim_end(),
                        expected_err.trim_end()
                    ),
                );
            }
            continue;
        }

        if !stdout_matches {
            // Output parity for a fixture that ran is a hard requirement.
            failures.push(format!(
                "{label}: output mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
                expected.trim_end(),
                actual.trim_end()
            ));
            continue;
        }
        passed += 1;
    }

    let mut grouped: Vec<_> = reasons.into_iter().collect();
    grouped.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
    let summary = grouped
        .iter()
        .take(15)
        .map(|(key, (count, example))| format!("{count:3} {key}\n      e.g. {example}"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        failures.is_empty(),
        "pure-MIR C output diverged for {} fixture(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        passed >= PASS_FLOOR,
        "pure-MIR -> C11 end-to-end coverage dropped: {passed} of {accepted} accepted fixtures \
         passed (floor {PASS_FLOOR}).\nRemaining emission reasons:\n{summary}"
    );
    let _ = fs::remove_dir_all(&dir);
    println!("pure-MIR -> C11: {passed}/{accepted} fixtures emit, compile and match their stdout");
    if std::env::var("AE_PURE_GATE_VERBOSE").is_ok() {
        for (key, (count, example)) in &grouped {
            println!("{count:3} {key}
      e.g. {example}");
        }
    }
}