//! P3.3 diagnostic-parity gate (ADR-0243 stage 2): a program the language
//! rejects must be rejected through the pure-MIR route with the *same*
//! message and position the `check` route reports.
//!
//! The negative corpora (`tests/ui`, `tests/types`, `tests/resolve`,
//! `tests/hir`) hold one rejection each. Both routes share the frontend, so a
//! divergence means the pure pipeline either accepts what the language
//! rejects, or replaces the language's diagnostic with a lowering message.
//! Either is a regression, never an allowlist entry.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn fixtures(dir: &str) -> Vec<PathBuf> {
    let root = repo_path(dir);
    let mut paths: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|err| panic!("read {dir}: {err}"))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ae"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn pure_route_reports_the_language_diagnostic() {
    let mut rejected = 0usize;
    let mut failures = Vec::new();
    let mut corpora = 0usize;

    for dir in ["tests/ui", "tests/types", "tests/resolve", "tests/hir"] {
        for path in fixtures(dir) {
            corpora += 1;
            let expected = match ae_compiler::frontend::check_path(&path) {
                Ok(()) => continue,
                Err(message) => message,
            };
            rejected += 1;
            // Every rejection names a position; a message without one cannot
            // drive an editor or a fixture comparison.
            let positioned = expected.rsplit(':').nth(2).is_some_and(|_| expected.matches(':').count() >= 3);
            if !positioned {
                failures.push(format!("{}: diagnostic has no position: {expected}", path.display()));
            }
            match ae_compiler::format_pure_mir_path(&path) {
                Ok(_) => failures.push(format!(
                    "{}: the language rejects this program but the pure route accepted it ({expected})",
                    path.display()
                )),
                Err(actual) if actual == expected => {}
                Err(actual) => failures.push(format!(
                    "{}: message differs\n  check: {expected}\n  pure:  {actual}",
                    path.display()
                )),
            }
        }
    }

    assert!(corpora >= 100, "expected the negative corpora, found {corpora} fixtures");
    assert!(rejected >= 100, "expected >=100 rejections, found {rejected}");
    assert!(
        failures.is_empty(),
        "{} negative fixture(s) diverge between the check and pure routes:\n{}",
        failures.len(),
        failures.join("\n")
    );
}