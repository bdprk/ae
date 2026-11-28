use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

fn run_ae_check(source: &str) -> (bool, String) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!("ae_p25_traitcheck_{}_{}.ae", std::process::id(), id));
    fs::write(&path, source).expect("write temp ae file");
    let out = Command::new(env!("CARGO_BIN_EXE_ae"))
        .arg("check")
        .arg(&path)
        .output()
        .expect("run ae check");
    let _ = fs::remove_file(&path);
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    (out.status.success(), stderr)
}

#[test]
fn unbounded_generic_method_call_rejected_at_declaration() {
    let src = "\
fn broken[T](x: T) -> i64 {
    return x.area();
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src);
    assert!(!ok, "expected declaration-site failure, got ok");
    assert!(err.contains("broken"), "diagnostic should name function: {err}");
    assert!(err.contains("'T'"), "diagnostic should name type param: {err}");
    assert!(err.contains("'area'"), "diagnostic should name method: {err}");
    assert!(err.contains("unbounded"), "diagnostic should mention unbounded: {err}");
}

#[test]
fn bounded_generic_missing_method_rejected_at_declaration() {
    let src = "\
trait Area {
    fn area(shape: T) -> i64;
}
fn broken[T: Area](shape: T) -> i64 {
    return shape.volume();
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src);
    assert!(!ok, "expected declaration-site failure, got ok");
    assert!(err.contains("broken"), "diagnostic should name function: {err}");
    assert!(err.contains("'T'"), "diagnostic should name type param: {err}");
    assert!(err.contains("'volume'"), "diagnostic should name missing method: {err}");
    assert!(err.contains("Area"), "diagnostic should name bounds: {err}");
}

#[test]
fn borrowed_type_param_receivers_are_checked_at_declaration() {
    let src_ref = "\
trait Area {
    fn area(shape: T) -> i64;
}
fn check_ref[T: Area](shape: &T) -> i64 {
    return shape.missing_ref();
}
fn main() {
    print(1);
}
";
    let (ok_ref, err_ref) = run_ae_check(src_ref);
    assert!(!ok_ref, "expected &T declaration-site failure");
    assert!(err_ref.contains("check_ref") && err_ref.contains("'missing_ref'") && err_ref.contains("Area"), "{err_ref}");

    let src_mut = "\
trait Area {
    fn area(shape: T) -> i64;
}
fn check_mut[T: Area](shape: &mut T) -> i64 {
    return shape.missing_mut();
}
fn main() {
    print(1);
}
";
    let (ok_mut, err_mut) = run_ae_check(src_mut);
    assert!(!ok_mut, "expected &mut T declaration-site failure");
    assert!(err_mut.contains("check_mut") && err_mut.contains("'missing_mut'") && err_mut.contains("Area"), "{err_mut}");
}

#[test]
fn local_bindings_typed_via_t_are_checked_at_declaration() {
    let src_annotated = "\
trait Area {
    fn area(shape: T) -> i64;
}
fn with_local[T: Area](shape: T) -> i64 {
    let copy: T = shape;
    return copy.perimeter();
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src_annotated);
    assert!(!ok, "expected annotated local binding failure");
    assert!(err.contains("with_local") && err.contains("'perimeter'") && err.contains("'T'") && err.contains("Area"), "{err}");

    let src_inferred = "\
trait CloneVal {
    fn clone_val(v: T) -> T;
}
fn with_inferred[T: CloneVal](v: T) -> i64 {
    let cloned = v.clone_val();
    return cloned.nope();
}
fn main() {
    print(1);
}
";
    let (ok2, err2) = run_ae_check(src_inferred);
    assert!(!ok2, "expected inferred local binding failure");
    assert!(err2.contains("with_inferred") && err2.contains("'nope'") && err2.contains("CloneVal"), "{err2}");

    let src_chained = "\
trait CloneVal {
    fn clone_val(v: T) -> T;
}
fn with_chain[T: CloneVal](v: T) -> i64 {
    return v.clone_val().nope();
}
fn main() {
    print(1);
}
";
    let (ok3, err3) = run_ae_check(src_chained);
    assert!(!ok3, "expected chained call failure");
    assert!(err3.contains("with_chain") && err3.contains("'nope'"), "{err3}");
}

#[test]
fn trait_method_arity_mismatch_rejected_at_declaration() {
    let src_method = "\
trait Calc {
    fn compute(self_val: T, a: i64, b: i64) -> i64;
}
fn bad_arity[T: Calc](x: T) -> i64 {
    return x.compute(1);
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src_method);
    assert!(!ok, "expected arity failure for method syntax");
    assert!(err.contains("bad_arity") && err.contains("compute") && err.contains("expected 2"), "{err}");

    let src_fn = "\
trait Calc {
    fn compute(self_val: T, a: i64, b: i64) -> i64;
}
fn bad_fn_arity[T: Calc](x: T) -> i64 {
    return compute(x, 1);
}
fn main() {
    print(1);
}
";
    let (ok2, err2) = run_ae_check(src_fn);
    assert!(!ok2, "expected arity failure for function syntax");
    assert!(err2.contains("bad_fn_arity") && err2.contains("compute") && err2.contains("expected 3"), "{err2}");
}

#[test]
fn ambiguous_trait_method_across_bounds_rejected_at_declaration() {
    let src_conflict = "\
trait TraitA {
    fn ping(x: T) -> i64;
}
trait TraitB {
    fn ping(x: T, n: i64) -> i64;
}
fn ambig[T: TraitA + TraitB](x: T) -> i64 {
    return x.ping();
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src_conflict);
    assert!(!ok, "expected ambiguity failure");
    assert!(err.contains("ambig") && err.contains("ambiguous") && err.contains("conflicting signatures"), "{err}");

    let src_dup = "\
trait TraitA {
    fn ping(x: T) -> i64;
}
trait TraitB {
    fn ping(x: T) -> i64;
}
fn ambig_same[T: TraitA + TraitB](x: T) -> i64 {
    return x.ping();
}
fn main() {
    print(1);
}
";
    let (ok2, err2) = run_ae_check(src_dup);
    assert!(!ok2, "expected ambiguity failure for duplicate method name");
    assert!(err2.contains("ambig_same") && err2.contains("ambiguous"), "{err2}");
}

#[test]
fn valid_unused_generic_trait_calls_and_shadowed_locals_pass() {
    let src = "\
trait Area {
    fn area(shape: T) -> i64;
}
trait Calc {
    fn compute(self_val: T, a: i64, b: i64) -> i64;
}
trait CloneVal {
    fn clone_val(v: T) -> T;
}
fn valid_unused[T: Area + Calc + CloneVal](x: T, n: i64) -> i64 {
    let y = x.clone_val();
    let shadowed = x;
    let shadowed = -5;
    return y.area() + x.compute(1, 2) + shadowed.abs() + n.abs();
}
fn main() {
    print(1);
}
";
    let (ok, err) = run_ae_check(src);
    assert!(ok, "expected valid unused generic to pass, got: {err}");
}
