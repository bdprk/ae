//! P3.3 calibration gate: the pure-MIR borrow checker must accept every
//! standalone program the existing compiler accepts, minus the shapes the
//! pure pipeline documents as unsupported (token cursors today). Divergences
//! here are either a lowering bug or a checker bug and must be fixed, not
//! silently allowlisted.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

#[test]
fn pure_mir_borrowck_agrees_on_the_accepted_run_corpus() {
    let run_dir = repo_path("tests/run");
    let mut paths: Vec<PathBuf> = fs::read_dir(&run_dir)
        .expect("read tests/run")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ae"))
        .collect();
    paths.sort();
    assert!(paths.len() > 50, "expected run corpus, found {}", paths.len());

    let mut checked = 0usize;
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for path in &paths {
        if ae_compiler::frontend::check_path(path).is_err() {
            continue;
        }
        match ae_compiler::compile_path_pure_mir_bodies(path) {
            Ok(_) => checked += 1,
            Err(err) if err.contains("pure MIR lowering cannot handle") => {
                skipped.push(format!("{}: {err}", path.display()));
            }
            Err(err) => failures.push(format!("{}: {err}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "pure-MIR borrowck rejected {} accepted fixtures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= 150,
        "expected >=150 calibrated fixtures, got {checked} (skipped {})",
        skipped.len()
    );
    assert!(
        skipped.len() <= 5,
        "unsupported-construct set grew to {} fixtures:\n{}",
        skipped.len(),
        skipped.join("\n")
    );
}