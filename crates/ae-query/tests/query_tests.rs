use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ae_query::{
    CacheLoadStatus, FileId, QueryDb, QueryError, QueryKey, QueryKind,
};

fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "ae_query_test_{name}_{}_{}.bin",
        std::process::id(),
        nanos
    ))
}

#[test]
fn m5_3_02_dependency_recording_captures_cross_file_edges() {
    let mut db = QueryDb::new();
    let file_a = FileId(1);
    let file_b = FileId(2);
    let file_c = FileId(3);

    db.set_source(
        file_a,
        "a.ae",
        "pub fn alpha(x: i64) -> i64 { return x + 1; }",
    );
    db.set_source(
        file_b,
        "b.ae",
        "use \"a.ae\";\npub fn beta(x: i64) -> i64 { return x; }",
    );
    db.set_source(
        file_c,
        "c.ae",
        "pub fn gamma() -> i64 { return 7; }",
    );
    db.register_import(file_b, file_c);

    let summary = db.check_file(file_b).expect("check_file(B) should succeed");
    assert_eq!(summary.file, file_b);
    assert_eq!(summary.imported_interfaces.len(), 2);

    let deps = db.dependencies_of(file_b);
    assert!(
        deps.contains(&QueryKey { kind: QueryKind::ParseFile, file: file_b }),
        "expected ParseFile(B) in dependencies_of(B): {deps:?}"
    );
    assert!(
        deps.contains(&QueryKey {
            kind: QueryKind::PublicInterfaceFingerprint,
            file: file_a,
        }),
        "expected PublicInterfaceFingerprint(A) from use header in dependencies_of(B): {deps:?}"
    );
    assert!(
        deps.contains(&QueryKey {
            kind: QueryKind::PublicInterfaceFingerprint,
            file: file_c,
        }),
        "expected PublicInterfaceFingerprint(C) from register_import in dependencies_of(B): {deps:?}"
    );
}

#[test]
fn m5_3_03_early_cutoff_on_private_body_and_whitespace_edits() {
    let mut db = QueryDb::new();
    let file_a = FileId(1);
    let file_b = FileId(2);

    db.set_source(
        file_a,
        "a.ae",
        r#"
// Public API
pub struct Point {
    x: i64,
    y: i64,
}

pub fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn private_helper(v: i64) -> i64 {
    return v * 2;
}
"#,
    );
    db.set_source(
        file_b,
        "b.ae",
        "pub fn use_a() -> i64 { return 1; }",
    );
    db.register_import(file_b, file_a);

    let check_b_key = QueryKey { kind: QueryKind::CheckFile, file: file_b };
    let initial_iface_a = db
        .public_interface_fingerprint(file_a)
        .expect("interface A");
    let _ = db.check_file(file_b).expect("initial check B");
    assert_eq!(db.recompute_count(check_b_key), 1);

    // Edit private function body, public function body, and comments/whitespace in A.
    db.set_source(
        file_a,
        "a.ae",
        r#"
// Updated comment with extra whitespace
pub struct Point {
    x: i64,
    y: i64,
}

pub fn add(a: i64, b: i64) -> i64 {
    let temp = a + b;
    return temp + 0;
}

fn private_helper(v: i64) -> i64 {
    return v * 999 + 42;
}
"#,
    );

    let updated_iface_a = db
        .public_interface_fingerprint(file_a)
        .expect("interface A after private edit");
    assert_eq!(
        initial_iface_a, updated_iface_a,
        "public_interface_fingerprint(A) must be unchanged on private-body/comment edits"
    );

    let _ = db.check_file(file_b).expect("check B after private edit");
    assert_eq!(
        db.recompute_count(check_b_key),
        1,
        "early cutoff: check_file(B) must have 0 recomputations after private edit in A"
    );

    // Now change a public function signature in A.
    db.set_source(
        file_a,
        "a.ae",
        r#"
pub struct Point {
    x: i64,
    y: i64,
}

pub fn add(a: i64, b: i32) -> i64 {
    return a;
}
"#,
    );

    let sig_changed_iface_a = db
        .public_interface_fingerprint(file_a)
        .expect("interface A after public fn signature change");
    assert_ne!(initial_iface_a, sig_changed_iface_a);

    let _ = db.check_file(file_b).expect("check B after public fn sig change");
    assert_eq!(
        db.recompute_count(check_b_key),
        2,
        "public fn signature change in A must trigger check_file(B) recomputation"
    );

    // Now change a public struct field in A.
    db.set_source(
        file_a,
        "a.ae",
        r#"
pub struct Point {
    x: i64,
    z: i64,
}

pub fn add(a: i64, b: i32) -> i64 {
    return a;
}
"#,
    );

    let struct_changed_iface_a = db
        .public_interface_fingerprint(file_a)
        .expect("interface A after struct field change");
    assert_ne!(sig_changed_iface_a, struct_changed_iface_a);

    let _ = db.check_file(file_b).expect("check B after struct field change");
    assert_eq!(
        db.recompute_count(check_b_key),
        3,
        "public struct field change in A must trigger check_file(B) recomputation"
    );
}

#[test]
fn m5_3_05_06_atomic_cache_roundtrip_and_corruption_recovery() {
    let cache_path = unique_temp_path("roundtrip");
    let _ = std::fs::remove_file(&cache_path);

    let file_a = FileId(10);
    let file_b = FileId(20);

    let mut db = QueryDb::new();
    db.set_source(file_a, "a.ae", "pub fn foo() -> i64 { return 1; }");
    db.set_source(file_b, "b.ae", "pub fn bar() -> i64 { return 2; }");
    db.register_import(file_b, file_a);

    let expected_check = db.check_file(file_b).expect("check_file(B)");
    let expected_rev = db.revision();
    let expected_deps = db.dependencies_of(file_b);

    db.save_cache(&cache_path).expect("save_cache should succeed");

    // Round-trip load.
    let (mut loaded_db, status) = QueryDb::load_cache(&cache_path).expect("load_cache");
    assert_eq!(status, CacheLoadStatus::Loaded);
    assert_eq!(loaded_db.revision(), expected_rev);
    assert_eq!(loaded_db.dependencies_of(file_b), expected_deps);

    let loaded_check = loaded_db.check_file(file_b).expect("cached check_file(B)");
    assert_eq!(loaded_check, expected_check);
    assert_eq!(
        loaded_db.recompute_count(QueryKey { kind: QueryKind::CheckFile, file: file_b }),
        0,
        "loaded cache should serve check_file(B) without recomputing"
    );

    // Truncated file recovery.
    let raw_bytes = std::fs::read(&cache_path).expect("read saved cache");
    assert!(raw_bytes.len() > 20);
    std::fs::write(&cache_path, &raw_bytes[..raw_bytes.len() / 2]).expect("write truncated");
    let (_, trunc_status) = QueryDb::load_cache(&cache_path).expect("load truncated");
    assert_eq!(trunc_status, CacheLoadStatus::CorruptedRecovered);

    // Bit-flipped file recovery.
    let mut flipped = raw_bytes.clone();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 0xFF;
    std::fs::write(&cache_path, &flipped).expect("write bit-flipped");
    let (_, flip_status) = QueryDb::load_cache(&cache_path).expect("load bit-flipped");
    assert_eq!(flip_status, CacheLoadStatus::CorruptedRecovered);

    // Missing file recovery.
    let _ = std::fs::remove_file(&cache_path);
    let (_, missing_status) = QueryDb::load_cache(&cache_path).expect("load missing");
    assert_eq!(missing_status, CacheLoadStatus::NotFound);
}

#[test]
fn unknown_file_returns_structured_error_and_noop_set_source_keeps_revision() {
    let mut db = QueryDb::new();
    let missing = FileId(999);
    assert_eq!(db.check_file(missing), Err(QueryError::UnknownFile(missing)));
    assert_eq!(
        db.public_interface_fingerprint(missing),
        Err(QueryError::UnknownFile(missing))
    );

    let f = FileId(1);
    db.set_source(f, "m.ae", "pub fn x() -> i64 { return 0; }");
    let rev1 = db.revision();
    db.set_source(f, "m.ae", "pub fn x() -> i64 { return 0; }");
    assert_eq!(db.revision(), rev1);
}
