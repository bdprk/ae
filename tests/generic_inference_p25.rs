use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn normalize_lines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn generic_borrow_slice_and_vec_inference_matches_across_backends() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("run")
        .join("generic_inference_p25.ae");
    let expected = normalize_lines(&fs::read(fixture.with_extension("stdout")).unwrap());
    for (label, codegen, interp) in [
        ("mir", Some("mir"), false),
        ("tree", Some("tree"), false),
        ("default", None, false),
        ("interp", None, true),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ae"));
        command.arg("run").arg(&fixture);
        if let Some(codegen) = codegen {
            command.env("AE_CODEGEN", codegen);
        }
        if interp {
            command.arg("--interp");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            normalize_lines(&output.stdout),
            expected,
            "mode {label} output differs"
        );
    }
}

#[test]
fn generic_borrow_slice_and_vec_inference_rejects_invalid_calls() {
    let cases = [
        // Repeated type parameter conflict on &[T]
        (
            "fn same_s[T](a: &[T], b: &[T]) -> i64 { return len(a) + len(b); } fn main() { let vi = vec[1, 2]; let vs = vec[\"a\"]; print(same_s(&vi, &vs)); }",
            "must be &[i64]",
        ),
        // Repeated type parameter conflict on vec[T]
        (
            "fn same_v[T](a: vec[T], b: vec[T]) -> i64 { return len(a) + len(b); } fn main() { let vi = vec[1]; let vs = vec[\"a\"]; print(same_v(vi, vs)); }",
            "argument 2 to 'same_v' must be vec[i64]",
        ),
        // Repeated type parameter conflict on &T
        (
            "struct A { x: i64 } struct B { x: i64 } fn same_b[T](a: &T, b: &T) -> i64 { return 0; } fn main() { let a = A { x: 1 }; let b = B { x: 2 }; print(same_b(&a, &b)); }",
            "argument must be &struct",
        ),
        // Fixed parameter still checked after inferring &[T]
        (
            "fn slice_plus[T](s: &[T], extra: i64) -> i64 { return len(s) + extra; } fn main() { let vi = vec[1, 2]; print(slice_plus(&vi, \"bad\")); }",
            "argument 2 to 'slice_plus' must be i64",
        ),
        // &mut T requires a mutable binding
        (
            "struct P { x: i64 } fn bump[T](p: &mut T) { p.x = p.x + 1; } fn main() { let p = P { x: 1 }; bump(&mut p); }",
            "cannot borrow 'p' mutably",
        ),
        // &mut [T] requires a mutable binding
        (
            "fn set0[T](s: &mut [T], v: T) { s[0] = v; } fn main() { let v = vec[1, 2]; set0(&mut v, 9); }",
            "cannot borrow 'v' mutably",
        ),
        // &mut T rejects aliasing mutable borrows
        (
            "struct P { x: i64 } fn both[T](a: &mut T, b: &mut T) {} fn main() { var p = P { x: 1 }; both(&mut p, &mut p); }",
            "cannot borrow 'p' mutably while it is borrowed",
        ),
        // &mut [T] rejects aliasing mutable borrows
        (
            "fn both[T](a: &mut [T], b: &mut [T]) {} fn main() { var v = vec[1, 2]; both(&mut v, &mut v); }",
            "cannot borrow 'v' mutably while it is borrowed",
        ),
        // &T on type without read-only borrow emits clear diagnostic
        (
            "fn f[T](x: &T) -> i64 { return 0; } fn main() { print(f(42)); }",
            "has no read-only borrow",
        ),
        // Non-vec passed to vec[T] cannot infer
        (
            "fn f[T](v: vec[T]) -> i64 { return len(v); } fn main() { print(f(42)); }",
            "write its type arguments",
        ),
        // Uninferable return-only type parameter
        (
            "fn make[T](x: i64) -> T { return x; } fn main() { print(make(1)); }",
            "write its type arguments",
        ),
        // Arity mismatch on bare generic call
        (
            "fn f[T](s: &[T]) -> i64 { return len(s); } fn main() { print(f()); }",
            "expects 1 arguments, got 0",
        ),
    ];

    for (source, expected) in cases {
        let path = std::env::temp_dir().join(format!(
            "ae-p25-infer-{}-{}.ae",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ae"))
            .arg("check")
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success(), "accepted invalid program: {source}");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(expected), "{source}: {err}");
    }
}
