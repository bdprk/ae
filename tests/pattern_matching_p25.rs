use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp_source(prefix: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ae-p25-patterns-{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.ae");
    fs::write(&path, source).unwrap();
    path
}

fn run_all_backends(prefix: &str, source: &str, expected_stdout: &str) {
    let path = temp_source(prefix, source);
    let ae = env!("CARGO_BIN_EXE_ae");
    for (label, codegen, interp) in [
        ("mir", Some("mir"), false),
        ("tree", Some("tree"), false),
        ("default", None, false),
        ("interp", None, true),
    ] {
        let mut command = Command::new(ae);
        command.arg("run").arg(&path);
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
        let actual = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        assert_eq!(actual, expected_stdout, "mode {label} stdout mismatch");
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

fn check_rejects(prefix: &str, source: &str) -> String {
    let path = temp_source(prefix, source);
    let output = Command::new(env!("CARGO_BIN_EXE_ae"))
        .args(["check"])
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
    assert!(!output.status.success(), "expected rejection for:\n{source}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn wildcard_match_arm_parity_across_backends() {
    let source = r#"
enum Color { Red, Green, Blue, Custom(i64, i64) }

fn classify(c: Color) -> i64 {
    match c {
        Color::Red => { return 1; }
        Color::Custom(r, _) => { return 100 + r; }
        _ => { return 0; }
    }
}

fn only_wildcard(c: Color) -> i64 {
    match c {
        _ => { return 42; }
    }
}

fn main() {
    print(classify(Color::Red));
    print(classify(Color::Green));
    print(classify(Color::Blue));
    print(classify(Color::Custom(7, 99)));
    print(only_wildcard(Color::Blue));
    var i = 0;
    while i < 3 {
        let c = Color::Green;
        match c {
            Color::Red => { print(999); }
            _ => {
                i = i + 1;
                continue;
            }
        }
    }
    print(i);
}
"#;
    run_all_backends("wildcard-parity", source, "1\n0\n0\n107\n42\n3\n");
}

#[test]
fn wildcard_exhaustiveness_and_redundancy_diagnostics() {
    // Redundant wildcard when all variants are already covered.
    let redundant = r#"
enum Flag { On, Off }
fn main() {
    let f = Flag::On;
    match f {
        Flag::On => { print(1); }
        Flag::Off => { print(0); }
        _ => { print(-1); }
    }
}
"#;
    let err = check_rejects("wildcard-redundant", redundant);
    assert!(err.contains("wildcard '_' is redundant"), "got: {err}");

    // Unreachable arm after wildcard.
    let after_wildcard = r#"
enum Flag { On, Off }
fn main() {
    let f = Flag::On;
    match f {
        Flag::On => { print(1); }
        _ => { print(0); }
        Flag::Off => { print(2); }
    }
}
"#;
    let err = check_rejects("arm-after-wildcard", after_wildcard);
    assert!(err.contains("unreachable match arm after wildcard '_'"), "got: {err}");
}

#[test]
fn nested_enum_destructuring_parity_across_backends() {
    let source = r#"
enum Inner { Val(i64), Text(string), Empty }
enum Outer { Wrap(Inner), Pair(Inner, Inner), None }
enum Deep { Box(Outer), Nil }

fn eval_outer(o: Outer) -> i64 {
    match o {
        Outer::Wrap(Inner::Val(x)) => { return x * 2; }
        Outer::Wrap(Inner::Text(s)) => {
            print(s);
            return len(s);
        }
        Outer::Wrap(Inner::Empty) => { return 0; }
        Outer::Pair(Inner::Val(a), Inner::Val(b)) => { return a + b; }
        Outer::Pair(_, Inner::Val(b)) => { return b; }
        _ => { return -1; }
    }
}

fn eval_deep(d: Deep) -> i64 {
    match d {
        Deep::Box(Outer::Wrap(Inner::Val(v))) => { return v + 10; }
        Deep::Box(_) => { return 1; }
        Deep::Nil => { return 0; }
    }
}

fn main() {
    print(eval_outer(Outer::Wrap(Inner::Val(21))));
    print(eval_outer(Outer::Wrap(Inner::Text("hello"))));
    print(eval_outer(Outer::Wrap(Inner::Empty)));
    print(eval_outer(Outer::Pair(Inner::Val(10), Inner::Val(20))));
    print(eval_outer(Outer::Pair(Inner::Empty, Inner::Val(55))));
    print(eval_outer(Outer::Pair(Inner::Val(3), Inner::Empty)));
    print(eval_outer(Outer::None));
    print(eval_deep(Deep::Box(Outer::Wrap(Inner::Val(5)))));
    print(eval_deep(Deep::Box(Outer::None)));
    print(eval_deep(Deep::Nil));
}
"#;
    run_all_backends(
        "nested-parity",
        source,
        "42\nhello\n5\n0\n30\n55\n-1\n-1\n15\n1\n0\n",
    );
}

#[test]
fn nested_generic_enum_and_vec_borrow_parity() {
    let source = r#"
enum Holder[T] { Full(T), Empty }

fn sum_nested(h: Holder[Option[i64]]) -> i64 {
    match h {
        Holder::Full(Option::Some(n)) => { return n; }
        Holder::Full(Option::None) => { return 0; }
        Holder::Empty => { return -1; }
    }
}

fn vec_nested(o: Option[vec[i64]]) -> i64 {
    match o {
        Option::Some(v) => {
            var acc = 0;
            for x in v {
                acc = acc + x;
            }
            return acc;
        }
        _ => { return 0; }
    }
}

fn main() {
    print(sum_nested(Holder[Option[i64]]::Full(Option[i64]::Some(99))));
    print(sum_nested(Holder[Option[i64]]::Full(Option[i64]::None)));
    print(sum_nested(Holder[Option[i64]]::Empty));

    let nums = vec[4, 5, 6];
    print(vec_nested(Option[vec[i64]]::Some(nums)));
    print(vec_nested(Option[vec[i64]]::None));
}
"#;
    run_all_backends("nested-generic-vec", source, "99\n0\n-1\n15\n0\n");
}

#[test]
fn nested_pattern_exhaustiveness_redundancy_and_ownership_rejections() {
    // Missing nested sub-variant without wildcard.
    let missing_inner = r#"
enum Inner { A(i64), B }
enum Outer { Wrap(Inner), None }
fn main() {
    let o = Outer::Wrap(Inner::A(1));
    match o {
        Outer::Wrap(Inner::A(x)) => { print(x); }
        Outer::None => { print(0); }
    }
}
"#;
    let err = check_rejects("nested-missing-inner", missing_inner);
    assert!(err.contains("match is not exhaustive"), "got: {err}");

    // Unreachable nested arm after Outer::Wrap(_).
    let covered_by_inner_wild = r#"
enum Inner { A(i64), B }
enum Outer { Wrap(Inner), None }
fn main() {
    let o = Outer::Wrap(Inner::A(1));
    match o {
        Outer::Wrap(_) => { print(1); }
        Outer::Wrap(Inner::A(x)) => { print(x); }
        Outer::None => { print(0); }
    }
}
"#;
    let err = check_rejects("nested-covered-by-wild", covered_by_inner_wild);
    assert!(err.contains("unreachable match arm"), "got: {err}");

    // Redundant wildcard when all nested combinations are already covered.
    let nested_redundant_wild = r#"
enum Inner { A(i64), B }
enum Outer { Wrap(Inner), None }
fn main() {
    let o = Outer::Wrap(Inner::A(1));
    match o {
        Outer::Wrap(Inner::A(x)) => { print(x); }
        Outer::Wrap(Inner::B) => { print(2); }
        Outer::None => { print(0); }
        _ => { print(-1); }
    }
}
"#;
    let err = check_rejects("nested-redundant-wild", nested_redundant_wild);
    assert!(err.contains("wildcard '_' is redundant"), "got: {err}");

    // Cannot move string view bound from a nested pattern.
    let move_nested_str = r#"
enum Inner { Text(string), Empty }
enum Outer { Wrap(Inner), None }
fn take(s: string) { print(s); }
fn main() {
    let o = Outer::Wrap(Inner::Text("hi"));
    match o {
        Outer::Wrap(Inner::Text(s)) => { take(s); }
        _ => {}
    }
}
"#;
    let err = check_rejects("nested-move-str-view", move_nested_str);
    assert!(!err.is_empty(), "expected move-out rejection");

    // Cannot reassign outer subject while matched in a nested arm.
    let assign_subject = r#"
enum Inner { Text(string), Empty }
enum Outer { Wrap(Inner), None }
fn main() {
    var o = Outer::Wrap(Inner::Text("hi"));
    match o {
        Outer::Wrap(Inner::Text(s)) => {
            o = Outer::None;
            print(s);
        }
        _ => {}
    }
}
"#;
    let err = check_rejects("nested-assign-subject", assign_subject);
    assert!(err.contains("while it is borrowed"), "got: {err}");

    // Cannot move a directly-bound non-Copy enum payload into a call.
    let move_inner_call = r#"
enum Inner { Text(string), Empty }
enum Outer { Wrap(Inner), None }
fn take(i: Inner) { match i { Inner::Text(s) => { print(s); } Inner::Empty => {} } }
fn main() {
    let s = "hello" + " world";
    let o = Outer::Wrap(Inner::Text(s));
    match o { Outer::Wrap(inner) => { take(inner); } Outer::None => {} }
}
"#;
    let err = check_rejects("direct-enum-payload-move-call", move_inner_call);
    assert!(err.contains("while it is borrowed"), "got: {err}");

    // Cannot return a directly-bound non-Copy enum payload.
    let move_inner_return = r#"
enum Inner { Text(string), Empty }
enum Outer { Wrap(Inner), None }
fn extract(o: Outer) -> Inner {
    match o {
        Outer::Wrap(inner) => { return inner; }
        Outer::None => { return Inner::Empty; }
    }
}
fn main() {}
"#;
    let err = check_rejects("direct-enum-payload-move-return", move_inner_return);
    assert!(err.contains("while it is borrowed"), "got: {err}");
}

#[test]
fn direct_non_copy_enum_payload_match_parity_across_backends() {
    let source = r#"
enum Inner { Text(string), Empty }
enum Outer { Wrap(Inner), None }

fn main() {
    let s = "hello" + " world";
    let o = Outer::Wrap(Inner::Text(s));
    match o {
        Outer::Wrap(inner) => {
            match inner {
                Inner::Text(t) => { print(t); }
                Inner::Empty => { print("empty"); }
            }
        }
        Outer::None => { print("none"); }
    }
}
"#;
    run_all_backends("direct-enum-payload-match", source, "hello world\n");
}
