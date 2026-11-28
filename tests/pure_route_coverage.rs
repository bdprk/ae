//! P3.3 stage 3 preparation (ADR-0243): the pure route must accept every
//! program the language accepts, not only the `tests/run` corpus.
//!
//! `tests/run` is covered by the calibration gate. This gate widens the
//! survey to the example programs and the standard library, which exercise
//! imports, generics and the hosted modules. A program the frontend accepts
//! must lower, validate and borrow-check through the pure pipeline; anything
//! else is a shape the route switch would reject, so it has to be fixed (or
//! the program removed) before `check`/`build`/`run` move to this checker.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn collect(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "ae") {
            out.push(path);
        }
    }
}

#[test]
fn pure_route_accepts_every_accepted_program() {
    let mut paths = Vec::new();
    collect(&repo_path("examples"), &mut paths);
    collect(&repo_path("std"), &mut paths);
    paths.sort();

    let mut accepted = 0usize;
    let mut failures = Vec::new();
    for path in &paths {
        if ae_compiler::frontend::check_path(path).is_err() {
            continue;
        }
        accepted += 1;
        if let Err(err) = ae_compiler::compile_path_pure_mir_bodies(path) {
            failures.push(format!("{}: {err}", path.display()));
        }
    }

    // The example programs (49) plus any stdlib module that stands alone.
    // Stdlib modules that need an import context are skipped by `check_path`
    // and covered by the module tests instead.
    assert!(accepted >= 45, "expected the example and stdlib corpus, found {accepted} accepted programs");
    assert!(
        failures.is_empty(),
        "{} of {accepted} accepted programs are rejected by the pure route:\n{}",
        failures.len(),
        failures.join("\n")
    );
}