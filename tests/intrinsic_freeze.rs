//! ADR-0224: the runtime-object intrinsic surface is frozen. New families must
//! be std modules over minimal primitives, not new compiler builtins.
use std::collections::BTreeSet;

fn compiler_intrinsics() -> BTreeSet<String> {
    ae_compiler::RUNTIME_OBJECT_INTRINSICS
        .iter()
        .map(|intrinsic| intrinsic.name().to_owned())
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

#[test]
fn registry_families_match_allowlist() {
    let allowlist_families: std::collections::BTreeMap<&str, &str> =
        include_str!("../docs/architecture/intrinsic-allowlist.txt")
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                let mut cols = l.split_whitespace();
                (cols.next().unwrap(), cols.next().unwrap())
            })
            .collect();
    for intrinsic in ae_compiler::RUNTIME_OBJECT_INTRINSICS {
        let expected = allowlist_families
            .get(intrinsic.name())
            .unwrap_or_else(|| panic!("{} missing from allowlist", intrinsic.name()));
        assert_eq!(
            intrinsic.family(),
            *expected,
            "family mismatch for {}",
            intrinsic.name()
        );
    }
}
