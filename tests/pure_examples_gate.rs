//! P3.2 examples differential gate: every `examples/*.ae` program must build
//! through the legacy pipeline and through the pure-MIR pipeline, and both
//! binaries must observe the same behavior: the same exit status, the same
//! stdout and (for error paths) the same positioned stderr.
//!
//! The corpus gate (`pure_e2e_corpus_gate.rs`) pins the accepted `tests/run`
//! fixtures with their `.stdout`/`.stderr`/`.exit` companions. This gate pins
//! the larger example programs, which exercise argv, stdin, filesystem trees,
//! child processes and the JSON reader. Each pair runs in its own fresh
//! sandbox seeded with the same files, so relative paths and directory
//! listings compare.
//!
//! The floor is 49 of 49 examples.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ae_compiler::ae_codegen_c::EmitConfig;
use ae_compiler::ae_layout::TargetSpec;
use ae_compiler::compile_path_pure_mir_to_c;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Examples the gate builds and runs at this slice. Raise with coverage;
/// never lower without a ledger entry.
const PASS_FLOOR: usize = 49;

const INPUT: &str = "alpha 3\nbeta 5\ngamma 8\nalpha line\n";

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Per-example argv and stdin. Examples that take a path or a pattern get a
/// seeded file; the rest run with an empty, closed stdin.
struct Case {
    args: Vec<&'static str>,
    stdin: &'static str,
}

fn case_for(name: &str) -> Case {
    match name {
        "word_count" | "sum_numbers" | "text_preview" => Case {
            args: vec!["input.txt"],
            stdin: "",
        },
        "grep_cli" => Case { args: vec!["alpha", "input.txt"], stdin: "" },
        "find_files" => Case { args: vec!["tree", "b"], stdin: "" },
        // The failure-to-start path is portable: every host reports it.
        "run_tool" => Case { args: vec!["definitely-not-a-program"], stdin: "" },
        "pipe_stats" => Case { args: vec![], stdin: "1\n2\n3\n4\n5\n7\n11\n" },
        _ => Case { args: vec![], stdin: "" },
    }
}

/// The same relative tree for both sandboxes: relative paths from argv, the
/// `grep` fixture's fixed input name, and a small directory for the walkers.
fn prepare(dir: &Path) {
    fs::create_dir_all(dir.join("tree/sub/deep")).expect("create tree");
    fs::write(dir.join("input.txt"), INPUT).expect("write input");
    fs::write(dir.join("bench-input.txt"), INPUT).expect("write bench input");
    fs::write(dir.join("tree/a.txt"), "one\n").expect("write a.txt");
    fs::write(dir.join("tree/sub/b.txt"), "two\n").expect("write b.txt");
    fs::write(dir.join("tree/sub/deep/c.txt"), "three\n").expect("write c.txt");
}

fn run_with(exe: &Path, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {} in {}: {err}", exe.display(), cwd.display()));
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    drop(child.stdin.take());
    child.wait_with_output().expect("wait for program")
}

fn normalize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Builds one example with the legacy CLI into `out` and starts it from the
/// sandbox, so compile warnings never mix into the program's stderr.
fn legacy_run(bin: &str, source: &Path, sandbox: &Path, case: &Case) -> Result<Output, String> {
    let out = sandbox.join(if cfg!(windows) { "legacy.exe" } else { "legacy" });
    let build = Command::new(bin)
        .arg("build")
        .arg(source)
        .arg("-o")
        .arg(&out)
        .output()
        .map_err(|err| format!("cannot run `ae build`: {err}"))?;
    if !build.status.success() {
        return Err(format!(
            "legacy build failed: {}",
            normalize(&build.stderr).lines().next().unwrap_or("").to_owned()
        ));
    }
    Ok(run_with(&out, sandbox, &case.args, case.stdin))
}

#[test]
#[ignore = "compiles 49 examples through both backends; run by the CI examples step"]
fn pure_mir_c11_examples_match_the_legacy_backend() {
    let mut paths: Vec<PathBuf> = fs::read_dir(repo_path("examples"))
        .expect("read examples")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ae"))
        .collect();
    paths.sort();

    let bin = env!("CARGO_BIN_EXE_ae");
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".to_owned());
    let config = EmitConfig { target: TargetSpec::host(), emit_main_wrapper: true };
    let root = std::env::temp_dir().join(format!("ae_examples_gate_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");

    let mut built = 0usize;
    let mut matched = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &paths {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("example")
            .to_owned();
        let case = case_for(&name);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pure_sandbox = root.join(format!("{id}_{name}_pure"));
        let legacy_sandbox = root.join(format!("{id}_{name}_legacy"));
        for sandbox in [&pure_sandbox, &legacy_sandbox] {
            fs::create_dir_all(sandbox).expect("create sandbox");
            prepare(sandbox);
        }

        let legacy = legacy_run(bin, path, &legacy_sandbox, &case);
        let legacy = match legacy {
            Ok(output) => output,
            Err(reason) => {
                failures.push(format!("{name}: {reason}"));
                continue;
            }
        };

        let source = match compile_path_pure_mir_to_c(path, &config) {
            Ok(source) => source,
            Err(err) => {
                failures.push(format!("{name}: emit failed: {err}"));
                continue;
            }
        };
        let c_path = pure_sandbox.join("program.c");
        let exe_path = pure_sandbox.join(if cfg!(windows) { "program.exe" } else { "program" });
        fs::write(&c_path, &source).expect("write generated C");
        let build = Command::new(&cc)
            .args(["-std=c11", "-O0", &c_path.to_string_lossy(), "-o", &exe_path.to_string_lossy(), "-lm"])
            .output()
            .expect("invoke host C compiler");
        if !build.status.success() {
            let first = String::from_utf8_lossy(&build.stderr)
                .lines()
                .find(|line| line.contains("error:"))
                .unwrap_or("C compile failed")
                .to_owned();
            failures.push(format!("{name}: pure C compile failed: {first}"));
            continue;
        }
        built += 1;

        let pure = run_with(&exe_path, &pure_sandbox, &case.args, case.stdin);
        let same_exit = pure.status.code() == legacy.status.code();
        let same_stdout = normalize(&pure.stdout) == normalize(&legacy.stdout);
        let same_stderr = normalize(&pure.stderr) == normalize(&legacy.stderr);
        if same_exit && same_stdout && same_stderr {
            matched += 1;
            continue;
        }
        failures.push(format!(
            "{name}: exit {} vs {}, stdout {} vs {}, stderr {} vs {}",
            pure.status.code().unwrap_or(-1),
            legacy.status.code().unwrap_or(-1),
            normalize(&pure.stdout).len(),
            normalize(&legacy.stdout).len(),
            normalize(&pure.stderr).len(),
            normalize(&legacy.stderr).len(),
        ));
        // Keep the first differing line, so the report names the real split.
        for (label, left, right) in [
            ("stdout", normalize(&pure.stdout), normalize(&legacy.stdout)),
            ("stderr", normalize(&pure.stderr), normalize(&legacy.stderr)),
        ] {
            if left != right {
                let l = left.lines().next().unwrap_or("");
                let r = right.lines().next().unwrap_or("");
                failures.push(format!("{name}: {label} first line: {l:?} vs {r:?}"));
            }
        }
    }

    let _ = fs::remove_dir_all(&root);
    assert!(built >= PASS_FLOOR, "pure C build coverage dropped: {built} of {} examples; failures: {failures:#?}", paths.len());
    assert!(
        failures.is_empty() && matched == paths.len(),
        "pure-MIR examples gate: {matched} of {} examples matched the legacy backend (floor {PASS_FLOOR});\nfailures: {failures:#?}",
        paths.len()
    );
}