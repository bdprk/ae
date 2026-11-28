use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(label: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ae_m3307_{label}_{}_{id}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn pack_test_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|(a, _)| *a);
    let mut out = Vec::new();
    out.extend_from_slice(b"AEAR1\0\0\0");
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for (path, data) in sorted {
        let path_bytes = path.as_bytes();
        out.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

struct Fixture {
    root: PathBuf,
    app_dir: PathBuf,
    cache_dir: PathBuf,
    archive_path: PathBuf,
    archive_bytes: Vec<u8>,
    archive_sha256: String,
    app_toml_sha256: String,
    app_main_sha256: String,
}

impl Fixture {
    fn create(label: &str) -> Self {
        let root = unique_temp_dir(label);
        let app_dir = root.join("app");
        let cache_dir = root.join("cache");
        fs::create_dir_all(app_dir.join("src")).unwrap();
        fs::create_dir_all(cache_dir.join("packages")).unwrap();

        let app_toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"0.1\"\n\n[dependencies]\nmath = { version = \"0.1.0\" }\n";
        let app_main = "use \"@math\";\nfn main() {\n    print(answer());\n}\n";
        fs::write(app_dir.join("ae.toml"), app_toml).unwrap();
        fs::write(app_dir.join("src/main.ae"), app_main).unwrap();

        let math_toml = b"[package]\nname = \"math\"\nversion = \"0.1.0\"\nae = \"0.1\"\n";
        let math_lib = b"pub fn answer() -> i64 { return 42; }\n";
        let archive_bytes = pack_test_archive(&[("ae.toml", math_toml), ("src/lib.ae", math_lib)]);
        let archive_sha256 = ae_compiler::sha256::hex(&archive_bytes);
        let archive_path = cache_dir.join("packages/math-0.1.0.aepkg");
        fs::write(&archive_path, &archive_bytes).unwrap();

        let app_toml_sha256 = ae_compiler::sha256::hex(app_toml.as_bytes());
        let app_main_sha256 = ae_compiler::sha256::hex(app_main.as_bytes());

        let fixture = Self {
            root,
            app_dir,
            cache_dir,
            archive_path,
            archive_bytes,
            archive_sha256,
            app_toml_sha256,
            app_main_sha256,
        };
        fixture.write_valid_lock();
        fixture
    }

    fn write_valid_lock(&self) {
        let lock = format!(
            "ae-lock-v1\narchive math 0.1.0 {}\nmanifest {} \"ae.toml\"\nsource {} \"src/main.ae\"\n",
            self.archive_sha256, self.app_toml_sha256, self.app_main_sha256
        );
        fs::write(self.app_dir.join("ae.lock"), lock).unwrap();
    }

    fn run_ae(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ae"))
            .current_dir(&self.app_dir)
            .env("AE_CACHE", &self.cache_dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn extracted_dir(&self) -> PathBuf {
        self.cache_dir
            .join("extracted")
            .join(format!("math-0.1.0-{}", self.archive_sha256))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn valid_cached_archive_verifies_extracts_and_builds_offline() {
    let fixture = Fixture::create("valid");

    let checked = fixture.run_ae(&["check", "--offline"]);
    assert!(
        checked.status.success(),
        "check --offline failed: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(fixture.extracted_dir().join("src/lib.ae").is_file());

    let exe_path = fixture.app_dir.join(if cfg!(windows) { "app.exe" } else { "app" });
    let built = fixture.run_ae(&["build", "--offline", "-o", exe_path.to_str().unwrap()]);
    assert!(
        built.status.success(),
        "build --offline failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(exe_path.is_file());

    let ran = Command::new(&exe_path).output().unwrap();
    assert!(ran.status.success());
    assert_eq!(String::from_utf8_lossy(&ran.stdout).replace("\r\n", "\n"), "42\n");
}

#[test]
fn tampered_one_byte_archive_fails_closed_before_extraction_or_compilation() {
    let fixture = Fixture::create("tampered");
    let mut tampered = fixture.archive_bytes.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let observed_sha = ae_compiler::sha256::hex(&tampered);
    fs::write(&fixture.archive_path, &tampered).unwrap();

    let exe_path = fixture.app_dir.join(if cfg!(windows) { "app.exe" } else { "app" });
    for args in [
        vec!["check", "--offline"],
        vec!["build", "--offline", "-o", exe_path.to_str().unwrap()],
    ] {
        let out = fixture.run_ae(&args);
        assert!(!out.status.success(), "expected failure for tampered archive");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("math"), "missing package name in stderr: {stderr}");
        assert!(
            stderr.contains("math-0.1.0.aepkg"),
            "missing archive path in stderr: {stderr}"
        );
        assert!(
            stderr.contains(&fixture.archive_sha256),
            "missing expected hash in stderr: {stderr}"
        );
        assert!(
            stderr.contains(&observed_sha),
            "missing observed hash in stderr: {stderr}"
        );
        assert!(
            !fixture.cache_dir.join("extracted").exists(),
            "extraction directory must not be created when verification fails"
        );
        assert!(!exe_path.exists(), "executable must not be created");
    }
}

#[test]
fn truncated_and_missing_archives_fail_closed_without_panicking() {
    let fixture = Fixture::create("truncated_missing");

    // 1. Truncated archive with original lock hash -> checksum mismatch.
    let mut truncated = fixture.archive_bytes.clone();
    truncated.truncate(truncated.len() - 3);
    fs::write(&fixture.archive_path, &truncated).unwrap();

    let exe_path = fixture.app_dir.join(if cfg!(windows) { "app.exe" } else { "app" });
    let out_trunc_hash = fixture.run_ae(&["build", "--offline", "-o", exe_path.to_str().unwrap()]);
    assert!(!out_trunc_hash.status.success());
    assert!(!fixture.cache_dir.join("extracted").exists());

    // 2. Truncated archive whose hash matches lock -> parser fails cleanly without panicking.
    let trunc_sha = ae_compiler::sha256::hex(&truncated);
    let lock = format!(
        "ae-lock-v1\narchive math 0.1.0 {}\nmanifest {} \"ae.toml\"\nsource {} \"src/main.ae\"\n",
        trunc_sha, fixture.app_toml_sha256, fixture.app_main_sha256
    );
    fs::write(fixture.app_dir.join("ae.lock"), lock).unwrap();
    let out_trunc_parse = fixture.run_ae(&["check", "--offline"]);
    assert!(!out_trunc_parse.status.success());
    let stderr_parse = String::from_utf8_lossy(&out_trunc_parse.stderr);
    assert!(stderr_parse.contains("truncated"), "{stderr_parse}");
    assert!(!fixture.cache_dir.join("extracted").exists());

    // 3. Missing archive -> fails closed with package name, path, expected hash, and <missing>.
    fixture.write_valid_lock();
    fs::remove_file(&fixture.archive_path).unwrap();
    let out_missing = fixture.run_ae(&["build", "--offline", "-o", exe_path.to_str().unwrap()]);
    assert!(!out_missing.status.success());
    let stderr_missing = String::from_utf8_lossy(&out_missing.stderr);
    assert!(stderr_missing.contains("math"), "{stderr_missing}");
    assert!(stderr_missing.contains("math-0.1.0.aepkg"), "{stderr_missing}");
    assert!(stderr_missing.contains(&fixture.archive_sha256), "{stderr_missing}");
    assert!(stderr_missing.contains("<missing>"), "{stderr_missing}");
    assert!(!fixture.cache_dir.join("extracted").exists());
}

#[test]
fn missing_or_malformed_lockfile_checksum_fails_closed() {
    let fixture = Fixture::create("bad_lock");

    // Malformed checksum in ae.lock.
    fs::write(
        fixture.app_dir.join("ae.lock"),
        format!(
            "ae-lock-v1\narchive math 0.1.0 not-a-sha256\nmanifest {} \"ae.toml\"\nsource {} \"src/main.ae\"\n",
            fixture.app_toml_sha256, fixture.app_main_sha256
        ),
    )
    .unwrap();
    let out_malformed = fixture.run_ae(&["check", "--offline"]);
    assert!(!out_malformed.status.success());
    let stderr_malformed = String::from_utf8_lossy(&out_malformed.stderr);
    assert!(stderr_malformed.contains("math"), "{stderr_malformed}");
    assert!(stderr_malformed.contains("not-a-sha256"), "{stderr_malformed}");
    assert!(!fixture.cache_dir.join("extracted").exists());

    // Missing checksum field in ae.lock.
    fs::write(
        fixture.app_dir.join("ae.lock"),
        format!(
            "ae-lock-v1\narchive math 0.1.0\nmanifest {} \"ae.toml\"\nsource {} \"src/main.ae\"\n",
            fixture.app_toml_sha256, fixture.app_main_sha256
        ),
    )
    .unwrap();
    let out_missing_field = fixture.run_ae(&["check", "--offline"]);
    assert!(!out_missing_field.status.success());
    assert!(!fixture.cache_dir.join("extracted").exists());

    // Missing ae.lock file altogether.
    fs::remove_file(fixture.app_dir.join("ae.lock")).unwrap();
    let out_no_lock = fixture.run_ae(&["build", "--offline", "-o", "app.exe"]);
    assert!(!out_no_lock.status.success());
    assert!(!fixture.cache_dir.join("extracted").exists());
}

#[test]
fn path_traversal_inside_verified_archive_is_rejected() {
    let fixture = Fixture::create("traversal");
    let evil_bytes = pack_test_archive(&[
        ("../escaped.ae", b"evil"),
        ("ae.toml", b"[package]\nname = \"math\"\nversion = \"0.1.0\"\nae = \"0.1\"\n"),
    ]);
    let evil_sha = ae_compiler::sha256::hex(&evil_bytes);
    fs::write(&fixture.archive_path, &evil_bytes).unwrap();
    fs::write(
        fixture.app_dir.join("ae.lock"),
        format!(
            "ae-lock-v1\narchive math 0.1.0 {evil_sha}\nmanifest {} \"ae.toml\"\nsource {} \"src/main.ae\"\n",
            fixture.app_toml_sha256, fixture.app_main_sha256
        ),
    )
    .unwrap();

    let out = fixture.run_ae(&["check", "--offline"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("escapes extraction root"), "{stderr}");
    assert!(!Path::new(&fixture.cache_dir.join("extracted/escaped.ae")).exists());
    assert!(!fixture.cache_dir.join("extracted").exists());
}
