//! M8.1.01: the frozen observable behavior matrix.
//!
//! `spec/conformance.md` maps every public contract frozen at this release to
//! the case that pins it. This gate fails when the heading is not frozen at
//! the current compiler version (a release must re-verify the matrix and bump
//! the line) and when a named case or fixture no longer exists.

use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn inline_code(line: &str) -> Vec<&str> {
    line.split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, span)| span)
        .collect()
}

#[test]
fn frozen_conformance_manifest_names_live_cases() {
    let path = repo().join("spec").join("conformance.md");
    let text = fs::read_to_string(&path).expect("read spec/conformance.md");
    let frozen = format!("v{}", env!("CARGO_PKG_VERSION"));
    assert!(
        text.lines().next().unwrap_or_default().contains(&frozen),
        "the matrix must be frozen at {frozen}; re-verify it and bump the heading when the version changes"
    );
    let mut referenced = 0usize;
    for (index, line) in text.lines().enumerate() {
        let row = index + 1;
        if !line.starts_with("| ") || line.starts_with("| ID") || line.starts_with("|---") {
            continue;
        }
        let mut cases = 0usize;
        for span in inline_code(line) {
            if let Some((file, function)) = span.split_once("::") {
                let path = repo().join(file);
                assert!(path.is_file(), "row {row} names a missing case file: {file}");
                let source = fs::read_to_string(&path).expect("read case file");
                assert!(source.contains(&format!("fn {function}(")), "row {row} names a missing case: {span}");
                cases += 1;
            } else if span.ends_with(".ae") {
                assert!(repo().join(span).is_file(), "row {row} names a missing fixture: {span}");
                cases += 1;
            }
        }
        assert!(cases > 0, "row {row} names no case: {line}");
        referenced += cases;
    }
    assert!(referenced >= 16, "the matrix is too small to be a conformance map: {referenced} case references");
}
