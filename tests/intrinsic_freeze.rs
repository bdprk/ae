//! ADR-0224: the runtime-object intrinsic surface is frozen. New families must
//! be std modules over minimal primitives, not new compiler builtins.
use std::collections::BTreeSet;

fn compiler_intrinsics() -> BTreeSet<String> {
    let src = include_str!("../crates/ae-compiler/src/frontend/lower_builtin.rs");
    let line = src
        .lines()
        .find(|l| l.contains("const RUNTIME_OBJECT_INTRINSICS"))
        .expect("runtime-object intrinsic dispatch line not found; update tests/intrinsic_freeze.rs");
    line.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| s.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()) && !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn allowlist() -> BTreeSet<String> {
    include_str!("../docs/architecture/intrinsic-allowlist.txt")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().next().unwrap().to_owned())
        .collect()
}

#[test]
fn no_new_runtime_object_intrinsics_without_an_adr() {
    let actual = compiler_intrinsics();
    let allowed = allowlist();
    let added: Vec<_> = actual.difference(&allowed).collect();
    assert!(
        added.is_empty(),
        "new compiler intrinsics {added:?} violate Hard Rule 2; write a std module over an existing primitive or open an ADR"
    );
}

#[test]
fn every_allowlisted_intrinsic_has_a_disposition() {
    for line in include_str!("../docs/architecture/intrinsic-allowlist.txt").lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let disposition = line.split_whitespace().nth(2).unwrap_or("");
        assert!(matches!(disposition, "keep" | "migrate" | "demote"), "bad disposition: {line}");
    }
}
