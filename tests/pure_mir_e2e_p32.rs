use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use ae_compiler::ae_codegen_c::EmitConfig;
use ae_compiler::ae_layout::TargetSpec;
use ae_compiler::ae_mir::{MirValue, eval_body};
use ae_compiler::{compile_path_pure_mir_bodies, compile_path_pure_mir_to_c};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("ae_p32_e2e_{prefix}_{pid}_{id}"));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn find_c_compiler() -> String {
    if let Ok(cc) = std::env::var("CC")
        && !cc.trim().is_empty()
    {
        return cc;
    }
    for candidate in ["gcc", "cc", "clang"] {
        if Command::new(candidate).arg("--version").output().is_ok() {
            return candidate.to_owned();
        }
    }
    panic!("no C compiler found (tried CC, gcc, cc, clang)");
}

fn compile_c_and_run(c_source: &str, name: &str, dir: &TempDirGuard) -> Output {
    let c_path = dir.path.join("program.c");
    let exe_name = if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    };
    let exe_path = dir.path.join(exe_name);

    fs::write(&c_path, c_source).expect("write generated C file");

    let cc = find_c_compiler();
    let compile = Command::new(&cc)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .arg("-lm")
        .output()
        .expect("invoke C compiler");

    assert!(
        compile.status.success(),
        "C11 compilation failed for {name}:\nstdout:\n{}\nstderr:\n{}\n--- C SOURCE ---\n{c_source}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    Command::new(&exe_path)
        .output()
        .expect("execute compiled C binary")
}

fn config_with_main() -> EmitConfig {
    EmitConfig {
        target: TargetSpec::host(),
        emit_main_wrapper: true,
    }
}

fn config_no_main() -> EmitConfig {
    EmitConfig {
        target: TargetSpec::host(),
        emit_main_wrapper: false,
    }
}

fn extract_first_i64_fn_name(c_src: &str) -> String {
    for line in c_src.lines() {
        if let Some(rest) = line.strip_prefix("int64_t ")
            && let Some(name) = rest.split('(').next()
            && line.ends_with("(void);")
        {
            return name.to_owned();
        }
    }
    panic!("could not find `int64_t <fn>(void);` declaration in:\n{c_src}");
}

fn assert_single_fn_parity(name: &str, source: &str, expected_i64: i64) {
    let dir = TempDirGuard::new(name);
    let ae_path = dir.path.join("main.ae");
    fs::write(&ae_path, source).expect("write AE source");

    let bodies = compile_path_pure_mir_bodies(&ae_path)
        .unwrap_or_else(|e| panic!("{name}: compile_path_pure_mir_bodies failed: {e}"));
    let compute_body = bodies
        .iter()
        .find(|b| b.name != "main")
        .unwrap_or_else(|| panic!("{name}: missing compute body"));

    let eval_val = eval_body(compute_body, &[])
        .unwrap_or_else(|e| panic!("{name}: eval_body failed: {e:?}"));
    assert_eq!(
        eval_val,
        MirValue::i64(expected_i64),
        "{name}: MIR evaluator mismatch"
    );

    // Also verify emit_main_wrapper: true produces a valid C11 program.
    let c_with_main = compile_path_pure_mir_to_c(&ae_path, &config_with_main())
        .unwrap_or_else(|e| panic!("{name}: compile_path_pure_mir_to_c (with main) failed: {e}"));
    let main_out = compile_c_and_run(&c_with_main, &format!("{name}_main"), &dir);
    assert_eq!(main_out.status.code(), Some(0));

    // Compile without main wrapper and invoke the computed function as C main's exit code.
    let mut c_src = compile_path_pure_mir_to_c(&ae_path, &config_no_main())
        .unwrap_or_else(|e| panic!("{name}: compile_path_pure_mir_to_c failed: {e}"));
    let fn_name = extract_first_i64_fn_name(&c_src);
    c_src.push_str(&format!("\nint main(void) {{ return (int){fn_name}(); }}\n"));

    let output = compile_c_and_run(&c_src, name, &dir);
    let exit_code = output.status.code().expect("exit code");
    assert_eq!(
        i64::from(exit_code),
        expected_i64,
        "{name}: compiled C exit code mismatch (MIR eval = {expected_i64})"
    );
}

#[test]
fn e2e_integer_arithmetic_and_comparisons() {
    assert_single_fn_parity(
        "arith_cmp",
        r#"
fn compute() -> i64 {
    let a = 10;
    let b = 3;
    let sum = a + b;
    let diff = a - b;
    let prod = a * b;
    let quot = a / b;
    let rem = a % b;
    var score = sum + diff + prod + quot + rem;
    if a > b && b >= 3 && a != b && !(a == b) && b < a && b <= 3 {
        score = score + 1;
    }
    return score;
}

fn main() {
    let _ = compute();
}
"#,
        13 + 7 + 30 + 3 + 1 + 1,
    );
}

#[test]
fn e2e_short_circuit_and_or() {
    assert_single_fn_parity(
        "short_circuit",
        r#"
fn compute() -> i64 {
    let zero = 0;
    var acc = 0;
    if false && (10 / zero == 1) {
        acc = acc + 100;
    } else {
        acc = acc + 10;
    }
    if true || (10 / zero == 1) {
        acc = acc + 20;
    }
    let a = true;
    let b = false;
    let c = (a && !b) || (b && a);
    if c {
        acc = acc + 5;
    }
    return acc;
}

fn main() {
    let _ = compute();
}
"#,
        35,
    );
}

#[test]
fn e2e_if_else_and_while_loops() {
    assert_single_fn_parity(
        "if_while",
        r#"
fn compute() -> i64 {
    var i = 1;
    var total = 0;
    while i <= 10 {
        if i % 2 == 0 {
            total = total + i;
        } else {
            total = total + 1;
        }
        i = i + 1;
    }
    return total;
}

fn main() {
    let _ = compute();
}
"#,
        // evens: 2+4+6+8+10 = 30, odds: 5*1 = 5 => 35
        35,
    );
}

#[test]
fn e2e_multi_function_helper_calls() {
    let dir = TempDirGuard::new("multi_fn");
    let ae_path = dir.path.join("main.ae");
    fs::write(
        &ae_path,
        r#"
fn compute() -> i64 {
    let a = add_scaled(3, 4, 2);
    let b = fib(7);
    return a + b;
}

fn add_scaled(x: i64, y: i64, scale: i64) -> i64 {
    return (x + y) * scale;
}

fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

fn main() {
    let _ = compute();
}
"#,
    )
    .expect("write AE source");

    let bodies = compile_path_pure_mir_bodies(&ae_path)
        .expect("compile_path_pure_mir_bodies multi_fn");
    assert_eq!(bodies.len(), 4);

    // Leaf helper `add_scaled` has no calls, so single-body MIR eval can verify it directly.
    let add_scaled_val = eval_body(&bodies[1], &[MirValue::i64(3), MirValue::i64(4), MirValue::i64(2)])
        .expect("eval add_scaled");
    assert_eq!(add_scaled_val, MirValue::i64(14));

    let mut c_src = compile_path_pure_mir_to_c(&ae_path, &config_no_main())
        .expect("compile_path_pure_mir_to_c multi_fn");
    let fn_name = extract_first_i64_fn_name(&c_src);
    c_src.push_str(&format!("\nint main(void) {{ return (int){fn_name}(); }}\n"));

    let output = compile_c_and_run(&c_src, "multi_fn", &dir);
    assert_eq!(output.status.code(), Some(14 + 13));
}

#[test]
fn e2e_structs_and_field_access() {
    assert_single_fn_parity(
        "structs",
        r#"
struct Point {
    x: i64,
    y: i64,
}

fn compute() -> i64 {
    let p = Point { x: 12, y: 30 };
    let sum = p.x + p.y;
    return sum;
}

fn main() {
    let _ = compute();
}
"#,
        42,
    );
}

#[test]
fn e2e_tagged_enums_and_match() {
    assert_single_fn_parity(
        "enums_match",
        r#"
enum Shape {
    Circle(i64),
    Rect(i64, i64),
    Empty,
}

fn compute() -> i64 {
    let s1 = Shape::Circle(7);
    let s2 = Shape::Rect(3, 5);
    let s3 = Shape::Empty;
    var total = 0;

    match s1 {
        Shape::Circle(r) => { total = total + r * 2; }
        Shape::Rect(w, h) => { total = total + w * h; }
        Shape::Empty => { total = total + 1; }
    }

    match s2 {
        Shape::Circle(r) => { total = total + r; }
        Shape::Rect(w, h) => { total = total + w * h; }
        Shape::Empty => { total = total + 1; }
    }

    match s3 {
        Shape::Circle(r) => { total = total + r; }
        Shape::Rect(w, h) => { total = total + w + h; }
        Shape::Empty => { total = total + 4; }
    }

    return total;
}

fn main() {
    let _ = compute();
}
"#,
        14 + 15 + 4,
    );
}

#[test]
fn negative_invalid_syntax_returns_err_without_panicking() {
    let dir = TempDirGuard::new("neg_syntax");
    let ae_path = dir.path.join("bad_syntax.ae");
    fs::write(&ae_path, "fn main( -> i64 { let x = ; }").expect("write bad syntax");

    let res = compile_path_pure_mir_to_c(&ae_path, &config_with_main());
    assert!(res.is_err(), "expected syntax error, got Ok");
}

#[test]
fn negative_use_after_move_returns_err_without_panicking() {
    let dir = TempDirGuard::new("neg_move");
    let ae_path = dir.path.join("use_after_move.ae");
    fs::write(
        &ae_path,
        r#"
struct Boxed {
    val: i64,
}

fn consume(b: Boxed) -> i64 {
    return b.val;
}

fn main() -> i64 {
    let b = Boxed { val: 10 };
    let first = consume(b);
    let second = consume(b);
    return first + second;
}
"#,
    )
    .expect("write use-after-move");

    let res = compile_path_pure_mir_to_c(&ae_path, &config_with_main());
    assert!(res.is_err(), "expected borrow/move error, got Ok");
}

#[test]
fn negative_missing_file_returns_err_without_panicking() {
    let dir = TempDirGuard::new("neg_missing");
    let ae_path = dir.path.join("does_not_exist.ae");
    let res = compile_path_pure_mir_to_c(&ae_path, &config_with_main());
    assert!(res.is_err(), "expected missing file error, got Ok");
}
