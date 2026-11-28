use ae_compiler::frontend::{check, compile, compile_with_context};

const PROGRAM_A: &str = r#"
/// Preconditions: size is a positive byte count; the block is released once.
extern "C" fn ae_std_alloc(size: i64, line: i64, column: i64) -> *mut i64;

/// Preconditions: x is a valid i64.
unsafe fn risky_a(x: i64) -> i64 {
    return x + 1;
}

/// Preconditions: x is a valid i64.
unsafe fn risky_b(x: i64) -> i64 {
    return x + 2;
}

/// Preconditions: x is a valid i64.
#[no_mangle]
extern "C" fn exported_a(x: i64) -> i64 {
    return x + 3;
}

fn main() {
    var v = 10;
    let raw = &mut v as *mut i64;
    unsafe {
        let _p = ae_std_alloc(8, 1, 1);
        let derefed = raw.deref();
        print(risky_a(derefed) + risky_b(1) + exported_a(2));
    }
}
"#;

const PROGRAM_B: &str = r#"
fn safe_0(x: i64) -> i64 {
    return x + 10;
}

fn safe_1(x: i64) -> i64 {
    return x + 20;
}

fn safe_2(x: i64) -> i64 {
    return x + 30;
}

fn safe_3(x: i64) -> i64 {
    return x + 40;
}

fn main() {
    print(safe_0(1) + safe_1(2) + safe_2(3) + safe_3(4));
}
"#;

fn assert_isolated_outputs(c_a: &str, c_b: &str) {
    assert!(c_a.contains("ae_std_alloc("), "Program A should emit its extern declaration");
    assert!(c_a.contains("exported_a("), "Program A should emit its #[no_mangle] exposure");

    assert!(
        !c_b.contains("ae_std_alloc("),
        "Program B must not inherit Program A's extern declaration at ID 0..3"
    );
    assert!(
        !c_b.contains("exported_a("),
        "Program B must not inherit Program A's exposure name at ID 0..3"
    );
    assert!(c_b.contains("ae_fn_0("), "Program B's function 0 should keep internal linkage");
    assert!(c_b.contains("ae_fn_1("), "Program B's function 1 should keep internal linkage");
    assert!(c_b.contains("ae_fn_2("), "Program B's function 2 should keep internal linkage");
    assert!(c_b.contains("ae_fn_3("), "Program B's function 3 should keep internal linkage");
}

#[test]
fn sequential_same_thread_compiles_are_isolated() {
    // Compile Program A (unsafe/extern/exposure/ptr at IDs 0..3), then Program B
    // (safe functions at IDs 0..3) repeatedly on the same thread.
    for _ in 0..4 {
        let (c_a, _ctx_a) = compile_with_context(PROGRAM_A).expect("Program A should compile");
        let (c_b, _ctx_b) = compile_with_context(PROGRAM_B).expect("Program B should compile without E0701");
        assert_isolated_outputs(&c_a, &c_b);

        // Also verify check() has zero cross-contamination.
        check(PROGRAM_A).expect("Program A check");
        check(PROGRAM_B).expect("Program B check");
    }
}

#[test]
fn concurrent_and_same_thread_compiles_are_isolated() {
    let (baseline_a, _) = compile_with_context(PROGRAM_A).expect("baseline A");
    let (baseline_b, _) = compile_with_context(PROGRAM_B).expect("baseline B");
    assert_isolated_outputs(&baseline_a, &baseline_b);

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(scope.spawn(|| {
                for _ in 0..4 {
                    let c_a = compile(PROGRAM_A).expect("Program A compiles concurrently");
                    let c_b = compile(PROGRAM_B).expect("Program B compiles concurrently");
                    assert_eq!(c_a, baseline_a, "Program A output drifted under concurrency");
                    assert_eq!(c_b, baseline_b, "Program B output drifted under concurrency");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread completed without panic");
        }
    });
}
