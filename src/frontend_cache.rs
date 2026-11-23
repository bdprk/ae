//! Frontend reuse cache: skips parsing, checking, and C generation when every
//! source file behind a program still has the bytes that produced the cached C.
//!
//! The cache stores one entry per entry file. An entry records the C text it
//! produced, the hash of that text, and the (digest, path) pair of every source
//! unit the compiler read. A later build re-reads and re-hashes those units and
//! serves the cached C only when all of them still match — no lexing, no
//! parsing, no type checking, no code generation.
//!
//! What this is not: there is no per-item granularity, no interface summary in
//! the key, and no partial rebuild. One changed byte anywhere in the import
//! graph runs the whole frontend, exactly as before. It removes the work of a
//! build that has nothing to do, and nothing more.

use std::{fmt::Write as _, fs, path::{Path, PathBuf}, time::UNIX_EPOCH};

use ae_compiler::{frontend, sha256};

/// Bumped when the entry layout changes; old entries then read as a miss
/// instead of being misparsed.
const FORMAT: &str = "1";

/// Identifying string for the compiler that produced an entry. A rebuilt
/// compiler keeps its version number during development, so the executable's
/// own size and modification time are part of the identity: without them a
/// codegen fix in the working tree would keep serving yesterday's C.
fn tool_identity(imports: &str) -> String {
    let exe = std::env::current_exe().ok();
    let (len, stamp) = match exe.as_deref().and_then(|p| fs::metadata(p).ok()) {
        Some(meta) => (
            meta.len(),
            meta.modified().ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
        None => (0, 0),
    };
    // A legacy-route compile (the temporary escape hatch) must never serve a
    // resolved-route request: the route mode is part of the identity, so the
    // deny measurements cannot be masked by an earlier legacy compile.
    let route = if std::env::var_os("AE_ALLOW_LEGACY_ROUTE").is_some() { "legacy" } else { "resolved" };
    format!("{} {len} {stamp} codegen:{} route:{route} imports:{imports}", env!("CARGO_PKG_VERSION"), frontend::codegen_mode())
}

/// Canonical form of the entry file, used both to name the entry directory and
/// to detect an entry that belongs to a different file.
fn canonical_root(root: &Path) -> Option<PathBuf> { fs::canonicalize(root).ok() }

fn entry_dir(cache_root: &Path, canonical: &Path) -> PathBuf {
    let name = sha256::hex(canonical.to_string_lossy().as_bytes());
    cache_root.join("frontend").join(name)
}

/// The query key for this program: a hash over every unit's label and digest in
/// sorted order, so the value depends on what was compiled and not on the order
/// the loader happened to visit files in.
fn query_key(units: &[frontend::SourceUnit], tool: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = units.iter()
        .map(|unit| (unit.label.as_str(), unit.digest.as_str()))
        .collect();
    pairs.sort_unstable();
    let extra: usize = pairs.iter().map(|(l, d)| l.len() + d.len() + 2).sum();
    let mut material = String::with_capacity(17 + tool.len() + extra);
    material.push_str("frontend-codegen\0");
    material.push_str(tool);
    for (label, digest) in pairs {
        material.push('\0');
        material.push_str(label);
        material.push('\0');
        material.push_str(digest);
    }
    sha256::hex(material.as_bytes())
}

/// Result of a successful lookup.
pub struct Hit {
    /// The full generated C, byte-identical to what a fresh compile produces.
    pub c: String,
    /// Query key of the entry that was served, for timing output.
    pub key: String,
    pub units: Vec<frontend::SourceUnit>,
}

/// Returns the cached C for `root` when every recorded source unit still hashes
/// to its recorded digest and the stored C still matches its own hash. Any
/// doubt — unreadable file, short entry, changed byte, different compiler — is
/// a miss, which the caller resolves by compiling from source.
pub fn lookup(cache_root: &Path, root: &Path, imports: &str) -> Option<Hit> {
    let canonical = canonical_root(root)?;
    let dir = entry_dir(cache_root, &canonical);
    let text = fs::read_to_string(dir.join("entry")).ok()?;
    let mut lines = text.lines();

    if lines.next()?.strip_prefix("format ")? != FORMAT { return None; }
    let tool = tool_identity(imports);
    if lines.next()?.strip_prefix("tool ")? != tool { return None; }
    let recorded_root = lines.next()?.strip_prefix("root ")?;
    if recorded_root != canonical.to_string_lossy().as_ref() { return None; }
    let key = lines.next()?.strip_prefix("key ")?.to_owned();
    let c_digest = lines.next()?.strip_prefix("c ")?.to_owned();

    let mut units: Vec<frontend::SourceUnit> = Vec::new();
    for line in lines {
        if line.is_empty() { continue; }
        // Tab-separated because labels and paths may both contain spaces.
        let mut parts = line.split('\t');
        if parts.next()? != "unit" { return None; }
        let digest = parts.next()?.to_owned();
        let label = parts.next()?.to_owned();
        let path = PathBuf::from(parts.next()?);
        // A recorded unit that can no longer be read means the program cannot
        // be reproduced from those files; compile from source and say nothing.
        if sha256::hex(&fs::read(&path).ok()?) != digest { return None; }
        units.push(frontend::SourceUnit { label, path, digest });
    }
    if units.is_empty() { return None; }
    if query_key(&units, &tool) != key { return None; }

    let c_path = dir.join("program.c");
    let bytes = fs::read(&c_path).ok()?;
    if sha256::hex(&bytes) != c_digest { return None; }
    let c = String::from_utf8(bytes).ok()?;
    Some(Hit { c, key, units })
}

/// Publishes an entry. Best effort by design: a failed cache write must never
/// turn a successful compile into a failed build, so every error here is
/// swallowed and the next build simply misses.
#[allow(dead_code)]
pub fn store(cache_root: &Path, root: &Path, imports: &str, tracked: &frontend::TrackedC) -> Option<String> {
    let c = format!("{}{}", tracked.runtime, tracked.user);
    store_c(cache_root, root, imports, tracked, &c)
}

pub(crate) fn store_c(cache_root: &Path, root: &Path, imports: &str, tracked: &frontend::TrackedC, c: &str) -> Option<String> {
    let canonical = canonical_root(root)?;
    let dir = entry_dir(cache_root, &canonical);
    let tool = tool_identity(imports);
    let key = query_key(&tracked.units, &tool);
    let c_digest = sha256::hex(c.as_bytes());

    fs::create_dir_all(&dir).ok()?;
    let stamp = std::process::id();

    let c_staging = dir.join(format!("program.c.{stamp}.tmp"));
    if fs::write(&c_staging, c).is_err() { let _ = fs::remove_file(&c_staging); return None; }

    let mut entry = String::with_capacity(128 + tracked.units.len() * 96);
    let _ = writeln!(entry, "format {FORMAT}\ntool {tool}\nroot {}\nkey {key}\nc {c_digest}", canonical.display());
    for unit in &tracked.units {
        let _ = writeln!(entry, "unit\t{}\t{}\t{}", unit.digest, unit.label, unit.path.display());
    }
    let e_staging = dir.join(format!("entry.{stamp}.tmp"));
    if fs::write(&e_staging, entry).is_err() {
        let _ = fs::remove_file(&c_staging);
        let _ = fs::remove_file(&e_staging);
        return None;
    }

    // C first, entry second: a crash in between leaves an entry that still
    // names an old C file, which the next lookup rejects on the digest.
    if fs::rename(&c_staging, dir.join("program.c")).is_err() {
        let _ = fs::remove_file(&c_staging);
        let _ = fs::remove_file(&e_staging);
        return None;
    }
    if fs::rename(&e_staging, dir.join("entry")).is_err() {
        let _ = fs::remove_file(&e_staging);
        return None;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let base = std::env::temp_dir();
            for _ in 0..1000 {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("ae-fc-{}-{n}", std::process::id()));
                if fs::create_dir(&path).is_ok() { return Self(path); }
            }
            panic!("cannot create temp dir");
        }
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for TempDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn tracked(files: &[&Path], c: &str) -> frontend::TrackedC {
        let units = files.iter().map(|p| frontend::SourceUnit {
            label: p.file_name().unwrap().to_string_lossy().into_owned(),
            path: p.to_path_buf(),
            digest: sha256::hex(&fs::read(p).unwrap()),
        }).collect();
        let (runtime, user) = c.split_at(c.len() / 2);
        frontend::TrackedC { runtime: runtime.to_owned(), user: user.to_owned(), units }
    }

    #[test]
    fn round_trip_serves_the_same_c() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        write(&src, "fn main() { print(\"hi\") }");
        let t = tracked(&[&src], "/*runtime*//*user*/");
        let key = store(tmp.path(), &src, "", &t).expect("stored");
        let hit = lookup(tmp.path(), &src, "").expect("hit");
        assert_eq!(hit.c, "/*runtime*//*user*/");
        assert_eq!(hit.key, key);
    }

    #[test]
    fn changed_unit_invalidates() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        write(&src, "fn main() { print(1) }");
        store(tmp.path(), &src, "", &tracked(&[&src], "c")).unwrap();
        write(&src, "fn main() { print(2) }");
        assert!(lookup(tmp.path(), &src, "").is_none());
    }

    #[test]
    fn missing_unit_invalidates() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        let lib = tmp.path().join("lib.ae");
        write(&src, "use \"lib.ae\";");
        write(&lib, "pub fn f() -> i64 { return 1 }");
        store(tmp.path(), &src, "", &tracked(&[&src, &lib], "c")).unwrap();
        fs::remove_file(&lib).unwrap();
        assert!(lookup(tmp.path(), &src, "").is_none());
    }

    #[test]
    fn damaged_c_invalidates() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        write(&src, "fn main() { print(1) }");
        store(tmp.path(), &src, "", &tracked(&[&src], "compiled")).unwrap();
        let dir = entry_dir(tmp.path(), &canonical_root(&src).unwrap());
        fs::write(dir.join("program.c"), "tampered").unwrap();
        assert!(lookup(tmp.path(), &src, "").is_none());
    }

    #[test]
    fn different_compiler_identity_invalidates() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        write(&src, "fn main() { print(1) }");
        store(tmp.path(), &src, "", &tracked(&[&src], "c")).unwrap();
        let dir = entry_dir(tmp.path(), &canonical_root(&src).unwrap());
        let entry = fs::read_to_string(dir.join("entry")).unwrap();
        let old = format!("tool {}\n", tool_identity(""));
        assert!(entry.contains(&old));
        fs::write(dir.join("entry"), entry.replace(&old, "tool 0 0 0\n")).unwrap();
        assert!(lookup(tmp.path(), &src, "").is_none());
    }

    #[test]
    fn entry_for_another_root_is_not_served() {
        let tmp = TempDir::new();
        let src = tmp.path().join("main.ae");
        let other = tmp.path().join("other.ae");
        write(&src, "fn main() { print(1) }");
        write(&other, "fn main() { print(1) }");
        store(tmp.path(), &src, "", &tracked(&[&src], "c")).unwrap();
        assert!(lookup(tmp.path(), &other, "").is_none());
    }

    #[test]
    fn key_ignores_unit_order() {
        let tmp = TempDir::new();
        let a = tmp.path().join("a.ae");
        let b = tmp.path().join("b.ae");
        write(&a, "a");
        write(&b, "b");
        let forward = tracked(&[&a, &b], "c");
        let mut reversed = tracked(&[&a, &b], "c");
        reversed.units.reverse();
        assert_eq!(query_key(&forward.units, "tool"), query_key(&reversed.units, "tool"));
    }
}
