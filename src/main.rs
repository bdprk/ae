use std::{env, ffi::{OsStr, OsString}, fs, path::{Path, PathBuf}, process::{Command, ExitCode}, time::Instant};

use ae_compiler::{fmt, frontend, harness, sha256};
mod package;
mod render;
mod frontend_cache;

const USAGE: &str = "usage: ae <check|emit-c|emit-tree|emit-cfg|emit-mir|emit-resolve|emit-types|emit-hir> [source.ae|project]\n       ae check --json [source.ae|project]\n       ae check --render [source.ae|project]\n       ae fmt <source.ae>\n       ae test [source.ae|project] [filter] [--interp]\n       ae run [source.ae|project] [--release|--interp] [-- <program args>]\n       ae build [source.ae|project] -o <output> [--debug]\n       ae cache [dir|clear|stats]\n       ae deps [source.ae|project]\n       ae new <directory>\n       ae init [directory]\n       ae doc [--test] [source.ae|project]\n       ae explain <E0000>\n       ae add <path>\n       ae remove <name>\n       ae update [project]\n       ae lsp\n       ae version";

/// A toolchain failure with the exit code it maps to. Usage errors exit 2 and
/// everything else exits 1, so a script can tell "you called it wrong" from
/// "your program is wrong" without parsing stderr.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self { code: 2, message: message.into() }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self { code: 1, message }
    }
}

/// Literal messages are command-line misuse (a bad path, a bad flag); compile
/// and build failures arrive as owned strings from the compiler and exit 1.
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::usage(message.to_owned())
    }
}

struct TempDir(PathBuf);
impl TempDir {
    fn create(parent: &Path) -> Result<Self, String> {
        for counter in 0..1000 {
            let path = parent.join(format!(".ae-{}-{counter}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create temporary directory in {}: {e}", parent.display())),
            }
        }
        Err("cannot allocate temporary directory".into())
    }
}
impl Drop for TempDir {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn cache_root() -> PathBuf {
    if let Some(dir) = env::var_os("AE_CACHE") { return PathBuf::from(dir); }
    if cfg!(windows) {
        if let Some(dir) = env::var_os("LOCALAPPDATA") { return PathBuf::from(dir).join("ae").join("cache"); }
    } else {
        if let Some(dir) = env::var_os("XDG_CACHE_HOME") { return PathBuf::from(dir).join("ae"); }
        if let Some(dir) = env::var_os("HOME") { return PathBuf::from(dir).join(".cache").join("ae"); }
    }
    env::temp_dir().join("ae-cache")
}

/// First line of the C compiler's --version output identifies the toolchain
/// inside cache keys. An unqueryable compiler yields None, and the caller then
/// disables the cache: reusing an entry we cannot attribute to a toolchain is
/// exactly the stale binary a cache must never serve.
fn cc_identity(cc: &OsStr) -> Option<String> {
    let out = Command::new(cc).arg("--version").output().ok()?;
    if !out.status.success() { return None; }
    let line = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_owned();
    if line.is_empty() { None } else { Some(line) }
}

/// A cache entry is usable only while its manifest still describes the bytes on
/// disk. Size plus content hash catches a truncated copy, a replaced file and
/// hand damage, which existence plus nonzero length did not.
fn cache_manifest_matches(entry: &Path, exe: &Path) -> bool {
    let Ok(manifest) = fs::read_to_string(entry.join("manifest")) else { return false };
    let Ok(bytes) = fs::read(exe) else { return false };
    manifest.trim_end() == format!("{} {}", bytes.len(), sha256::hex(&bytes))
}

/// Publishes an entry with the executable renamed into place before its
/// manifest: a crash between the two leaves a manifest that does not match,
/// which the next build reads as a miss rather than as a usable entry.
fn publish_cache(entry: &Path, exe: &Path) {
    let Ok(bytes) = fs::read(exe) else { return };
    let Some(name) = exe.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    if fs::create_dir_all(entry).is_err() { return; }
    let staging = entry.join(format!("{name}.{}.tmp", std::process::id()));
    if fs::write(&staging, &bytes).is_err() {
        let _ = fs::remove_file(&staging);
        return;
    }
    // The entry is an executable: `fs::write` creates it 0644, and a cache
    // hit copies those permissions verbatim, so the bit is set before the
    // rename publishes it. Windows has no executable bit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&staging, fs::Permissions::from_mode(0o755));
    }
    let manifest = entry.join(format!("manifest.{}.tmp", std::process::id()));
    if fs::write(&manifest, format!("{} {}", bytes.len(), sha256::hex(&bytes))).is_err() {
        let _ = fs::remove_file(&staging);
        return;
    }
    if fs::rename(&staging, entry.join(&name)).is_err() {
        let _ = fs::remove_file(&staging);
        let _ = fs::remove_file(&manifest);
        return;
    }
    let _ = fs::rename(&manifest, entry.join("manifest"));
}

/// Cache size cap from AE_CACHE_LIMIT, in bytes. Zero or an unparseable
/// value means "no cap": a limit nobody asked for is worse than a growing
/// cache, and a typo must not silently delete entries.
fn cache_limit() -> Option<u64> {
    let raw = env::var_os("AE_CACHE_LIMIT")?;
    raw.to_str()?.trim().parse::<u64>().ok().filter(|n| *n > 0)
}

/// Sum of the regular files directly inside one cache entry directory. Both
/// layouts (executable plus manifest, generated C plus entry) are flat, so a
/// recursive walk would only add failure modes.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(read) = fs::read_dir(dir) else { return 0 };
    read.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Records a use so pruning can tell hot entries from cold ones. The marker
/// file's modification time is the clock; its contents would be a second
/// source of truth to keep in sync.
fn touch_used(entry: &Path) { let _ = fs::write(entry.join("used"), b""); }

/// Last use of an entry directory. The marker file exists only for executable
/// entries; for the frontend entries the directory's own timestamp is the
/// clock, which moves whenever an entry file is rewritten.
fn dir_used(dir: &Path) -> std::time::SystemTime {
    fs::metadata(dir.join("used")).and_then(|m| m.modified())
        .or_else(|_| fs::metadata(dir).and_then(|m| m.modified()))
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Drops least recently used entries until the cache fits the limit. Both the
/// executable cache and the frontend cache share the budget: a limit that
/// counted only one of them would not bound the disk this tool uses. Runs after
/// publishing; a delete that fails is not a build failure — the entry simply
/// survives to the next prune.
fn prune_cache(limit: u64) {
    let root = cache_root();
    let mut found: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for sub in ["entries", "frontend"] {
        let Ok(read) = fs::read_dir(root.join(sub)) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let (bytes, used) = (dir_bytes(&path), dir_used(&path));
            found.push((path, bytes, used));
        }
    }
    let mut total: u64 = found.iter().map(|(_, size, _)| size).sum();
    if total <= limit { return; }
    found.sort_by_key(|(_, _, used)| *used);
    for (path, size, _) in found {
        if total <= limit { break; }
        if fs::remove_dir_all(&path).is_ok() { total = total.saturating_sub(size); }
    }
}

fn timings_enabled() -> bool { env::var_os("AE_TIMINGS").as_deref() == Some(std::ffi::OsStr::new("1")) }

/// Escapes a string for JSON output. The project has no dependencies by
/// design, so the escaping is spelled out: quotes, backslashes (paths on
/// Windows are full of them), and control characters.
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Splits one fail-fast diagnostic into its structured parts. The format is
/// `file:line:column: message` (or `line:column: message` for sources with no
/// file name); a message that does not parse becomes a whole-string
/// diagnostic rather than a mangled one.
fn diagnostic_json(message: &str) -> String {
    let (at, rest) = match message.split_once(": ") {
        Some((at, rest)) => (at, rest),
        None => return format!("{{\"file\":null,\"line\":null,\"column\":null,\"message\":\"{}\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}}", json_escape(message)),
    };
    let parts: Vec<&str> = at.split(':').collect();
    let (file, line, column): (Option<String>, Option<usize>, Option<usize>) = match parts.as_slice() {
        [file, line, column] => (Some((*file).to_owned()), line.parse().ok(), column.parse().ok()),
        // A Windows drive letter splits the path into two pieces
        // (`D` + `\用户\...`); rejoin them.
        [drive, file, line, column] if drive.len() == 1 && (file.starts_with('\\') || file.starts_with('/')) =>
            (Some(format!("{drive}:{file}")), line.parse().ok(), column.parse().ok()),
        [line, column] => (None, line.parse().ok(), column.parse().ok()),
        _ => (None, None, None),
    };
    // A location that does not parse is a formatting bug or a foreign
    // message; reporting the whole text beats reporting a mangled position.
    let (Some(line), Some(column)) = (line, column) else {
        return format!("{{\"file\":null,\"line\":null,\"column\":null,\"message\":\"{}\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}}", json_escape(message));
    };
    format!("{{\"file\":{},\"line\":{line},\"column\":{column},\"message\":\"{}\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}}",
        file.as_ref().map_or("null".to_owned(), |f| format!("\"{}\"", json_escape(f))),
        json_escape(rest))
}

fn structured_diagnostic_json(diagnostic: &ae_compiler::frontend::FrontendDiagnostic) -> String {
    let location = diagnostic.primary.as_ref().map(|label| &label.span);
    let file = location.and_then(|span| span.file.as_ref())
        .map_or("null".to_owned(), |file| format!("\"{}\"", json_escape(file)));
    let line = location.map_or("null".to_owned(), |span| span.line.to_string());
    let column = location.map_or("null".to_owned(), |span| span.column.to_string());
    let byte_start = location.map_or("null".to_owned(), |span| span.range.start.to_string());
    let byte_end = location.map_or("null".to_owned(), |span| span.range.end.to_string());
    let code = diagnostic.code.as_ref().map_or("null".to_owned(), |code| format!("\"{}\"", json_escape(code)));
    let suggestions = diagnostic.suggestions.iter().map(|s| format!("\"{}\"", json_escape(s))).collect::<Vec<_>>().join(",");
    let secondary = diagnostic.secondary.iter().map(|label| {
        let span = &label.span;
        let file = span.file.as_ref().map_or("null".to_owned(), |file| format!("\"{}\"", json_escape(file)));
        let message = label.message.as_ref().map_or("null".to_owned(), |message| format!("\"{}\"", json_escape(message)));
        format!("{{\"file\":{file},\"line\":{},\"column\":{},\"byte_start\":{},\"byte_end\":{},\"message\":{message}}}", span.line, span.column, span.range.start, span.range.end)
    }).collect::<Vec<_>>().join(",");
    format!("{{\"file\":{file},\"line\":{line},\"column\":{column},\"message\":\"{}\",\"byte_start\":{byte_start},\"byte_end\":{byte_end},\"code\":{code},\"suggestions\":[{suggestions}],\"secondary\":[{secondary}]}}", json_escape(&diagnostic.message))
}

/// Build the generated C. Results are cached under a key covering every
/// input that can change the executable: generated C text, optimization
/// mode, C compiler identity, compiler version, and target platform.
fn build_c(c: &str, dir: &Path, release: bool, use_cache: bool) -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) { "program.exe" } else { "program" };
    let cfile = dir.join("program.c");
    let exe = dir.join(exe_name);
    fs::write(&cfile, c).map_err(|e| format!("writing generated C: {e}"))?;
    let cc = env::var_os("AE_CC").unwrap_or_else(|| "gcc".into());
    let optimization = if release { "-O2" } else { "-O0" };

    let mut cache_entry: Option<PathBuf> = None;
    if use_cache {
        let cc_name = cc.to_string_lossy().into_owned();
        match cc_identity(&cc) {
            None => {
                // Without a toolchain identity the key cannot distinguish
                // compilers, so the safe answer is to compile every time.
                if timings_enabled() { eprintln!("ae cache: disabled (cannot identify the C compiler {cc_name:?})"); }
            }
            Some(identity) => {
                let key = sha256::hex_parts(&[
                    c.as_bytes(),
                    optimization.as_bytes(),
                    identity.as_bytes(),
                    cc_name.as_bytes(),
                    env!("CARGO_PKG_VERSION").as_bytes(),
                    if cfg!(windows) { &b"windows"[..] } else { &b"unix"[..] },
                ]);
                let entry = cache_root().join("entries").join(key);
                let cached = entry.join(exe_name);
                if cached.is_file() && cache_manifest_matches(&entry, &cached) {
                    let started = Instant::now();
                    touch_used(&entry);
                    fs::copy(&cached, &exe).map_err(|e| format!("copying cached executable: {e}"))?;
                    // `fs::copy` carries the entry's permissions; an entry
                    // published before the executable bit was set would copy
                    // 0644, so heal the destination before running it.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o755));
                    }
                    if timings_enabled() {
                        eprintln!("ae cache: hit");
                        eprintln!("ae timing: c_compile_and_link_ms={:.3} (cache hit)", started.elapsed().as_secs_f64() * 1000.0);
                    }
                    return Ok(exe);
                }
                // The key covers every input, so an entry that fails its
                // manifest is damaged, not a near-miss: drop it and rebuild.
                if entry.exists() { let _ = fs::remove_dir_all(&entry); }
                cache_entry = Some(entry);
            }
        }
    }

    let started = Instant::now();
    // Run the compiler *inside* the temporary directory with bare file names.
    // On Windows the MinGW toolchain converts the command line to the ANSI code
    // page, so an absolute path holding characters outside that code page (a
    // directory named 用户, for example) reaches the linker mangled and the
    // build dies with "cannot open output file". Relative names keep the
    // command line ASCII whatever the project or output path is.
    // Math builtins (ADR-60) call libm directly. MinGW serves those symbols
    // from the C runtime; glibc older than 2.35 needs an explicit -lm, so the
    // Linux link line carries it (a no-op stub where libm merged into libc).
    let mut command = Command::new(&cc);
    command.current_dir(dir).arg("-std=c11").arg(optimization)
        .arg("program.c").arg("-o").arg(exe_name);
    if !cfg!(windows) { command.arg("-lm"); }
    let status = command.status().map_err(|e| format!("cannot start C compiler {:?}: {e}; set AE_CC to its executable", cc))?;
    if timings_enabled() {
        eprintln!("ae cache: miss");
        eprintln!("ae timing: c_compile_and_link_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
    }
    if !status.success() { return Err(format!("C compiler failed: {status}")); }

    // Publishing is best effort: nothing here may turn a successful compile
    // into a failure, and a half-written entry must read as a miss.
    if let Some(entry) = cache_entry {
        publish_cache(&entry, &exe);
        if let Some(limit) = cache_limit() { prune_cache(limit); }
    }
    Ok(exe)
}

fn cache_command(subcommand: Option<&OsString>) -> Result<u8, String> {
    let entries = cache_root().join("entries");
    let frontend_entries = cache_root().join("frontend");
    match subcommand.and_then(|s| s.to_str()) {
        None | Some("stats") => {
            let mut count = 0usize;
            let mut bytes = 0u64;
            if let Ok(read) = fs::read_dir(&entries) {
                for entry in read.flatten() {
                    if let Ok(meta) = fs::metadata(entry.path().join(if cfg!(windows) { "program.exe" } else { "program" })) {
                        count += 1;
                        bytes += meta.len();
                    }
                }
            }
            let mut frontend_count = 0usize;
            let mut frontend_bytes = 0u64;
            if let Ok(read) = fs::read_dir(&frontend_entries) {
                for entry in read.flatten() {
                    if let Ok(meta) = fs::metadata(entry.path().join("program.c")) {
                        frontend_count += 1;
                        frontend_bytes += meta.len();
                    }
                }
            }
            println!("entries: {count}");
            println!("bytes: {bytes}");
            println!("frontend entries: {frontend_count}");
            println!("frontend bytes: {frontend_bytes}");
            println!("limit: {}", cache_limit().map(|n| n.to_string()).unwrap_or_else(|| "none".into()));
            println!("dir: {}", entries.display());
            Ok(0)
        }
        Some("dir") => { println!("{}", entries.display()); Ok(0) }
        Some("clear") => {
            // Both caches are cleared together: leaving the frontend entries
            // behind would let a "cleared" cache still skip compilation.
            let mut cleared = false;
            for dir in [&entries, &frontend_entries] {
                match fs::remove_dir_all(dir) {
                    Ok(()) => { cleared = true; println!("cleared {}", dir.display()); }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("clearing {}: {e}", dir.display())),
                }
            }
            if !cleared { println!("cache is already empty"); }
            Ok(0)
        }
        Some(other) => Err(format!("unknown cache subcommand '{other}'; expected dir, clear, or stats\n{USAGE}")),
    }
}

fn run(args: Vec<OsString>) -> Result<u8, Failure> {
    if args.is_empty() { return Err(Failure::usage(USAGE)); }
    let cmd = args[0].to_str().ok_or(Failure::usage("command must be UTF-8"))?.to_owned();
    const COMMANDS: [&str; 23] = [
        "check", "emit-c", "emit-tree", "emit-cfg", "emit-mir", "emit-resolve", "emit-types", "emit-hir", "run", "build", "test",
        "fmt", "cache", "new", "init", "update", "deps", "version", "doc", "explain",
        "add", "remove", "lsp",
    ];
    if !COMMANDS.contains(&cmd.as_str()) {
        return Err(Failure::usage(format!("unknown command: {cmd}\n{USAGE}")));
    }
    if cmd == "lsp" {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        ae_compiler::lsp::run_lsp_loop(stdin.lock(), stdout.lock())
            .map_err(|e| Failure { code: 1, message: format!("lsp error: {e}") })?;
        return Ok(0);
    }
    if cmd == "version" {
        if args.len() != 1 { return Err(Failure::usage(USAGE)); }
        println!("ae {} ({}, {})", env!("CARGO_PKG_VERSION"),
            if cfg!(windows) { "windows" } else { "unix" },
            if cfg!(target_arch = "x86_64") { "x86-64" } else { "other-arch" });
        return Ok(0);
    }
    if cmd == "new" {
        if args.len() != 2 { return Err(Failure::usage(USAGE)); }
        let directory = Path::new(&args[1]);
        package::create_project(directory)?;
        println!("created package in {}", directory.display());
        return Ok(0);
    }
    if cmd == "add" {
        if args.len() != 2 { return Err(Failure::usage(USAGE)); }
        let manifest = package::discover_manifest()?;
        println!("{}", package::add_dependency(&manifest, Path::new(&args[1]))?);
        return Ok(0);
    }
    if cmd == "remove" {
        if args.len() != 2 { return Err(Failure::usage(USAGE)); }
        let alias = args[1].to_str().ok_or(Failure::usage("dependency name must be UTF-8"))?;
        let manifest = package::discover_manifest()?;
        println!("{}", package::remove_dependency(&manifest, alias)?);
        return Ok(0);
    }
    if cmd == "explain" {
        if args.len() != 2 { return Err(Failure::usage(USAGE)); }
        let code = args[1].to_str()
            .ok_or(Failure::usage("error code must be UTF-8"))?
            .to_ascii_uppercase();
        return match frontend::explain(&code) {
            Some(text) => { println!("{code}: {text}"); Ok(0) }
            None => Err(Failure::usage(format!("unknown error code '{code}'; run ae explain with a code from a diagnostic\n{USAGE}"))),
        };
    }
    if cmd == "doc" {
        let mut test = false;
        let mut rest: Vec<&OsString> = Vec::new();
        for arg in args.iter().skip(1) {
            if arg.to_str() == Some("--test") { test = true; } else { rest.push(arg); }
        }
        if rest.len() > 1 { return Err(Failure::usage(USAGE)); }
        let entry = match rest.first() {
            Some(input) => package::entry_path(Path::new(input))?,
            None => package::entry_path(&package::discover_manifest()?)?,
        };
        let source = fs::read_to_string(&entry.path)
            .map_err(|error| format!("{}: {error}", entry.path.display()))?;
        return if test { doc_test(&entry.path, &source) } else { doc_render(&entry.path, &source) };
    }
    if cmd == "init" {
        if args.len() > 2 { return Err(Failure::usage(USAGE)); }
        let directory = args.get(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
        package::init_project(&directory)?;
        println!("initialized package in {}", directory.display());
        return Ok(0);
    }
    if cmd == "cache" { return cache_command(args.get(1)).map_err(Failure::from); }
    if cmd == "fmt" {
        if args.len() != 2 { return Err(Failure::usage(USAGE)); }
        let started = Instant::now();
        let changed = fmt::fmt_path(Path::new(&args[1]))?;
        if timings_enabled() {
            eprintln!("ae timing: fmt_ms={:.3} (changed={changed})", started.elapsed().as_secs_f64() * 1000.0);
        }
        return Ok(0);
    }
    if cmd == "test" {
        let mut use_interp = false;
        let mut rest: Vec<&OsString> = Vec::new();
        for arg in args.iter().skip(1) {
            if arg.to_str() == Some("--interp") {
                use_interp = true;
            } else {
                rest.push(arg);
            }
        }
        if rest.len() > 2 { return Err(Failure::usage(USAGE)); }
        let filter = match rest.get(1).and_then(|a| a.to_str()) {
            Some(filter) => Some(filter.to_owned()),
            // A non-UTF-8 filter cannot match any UTF-8 test name; say so
            // instead of silently running everything.
            None if rest.len() == 2 => return Err(Failure::usage("test filter must be valid UTF-8")),
            None => None,
        };
        let input = match rest.first() {
            Some(input) => PathBuf::from(input),
            None => package::discover_manifest()?,
        };
        let is_package = package::manifest_path(&input).is_some();
        let package::PackageEntry { mut path, imports, .. } = package::entry_path(&input)?;
        if is_package {
            path.set_file_name("test.ae");
        }
        return run_tests(&path, filter.as_deref(), use_interp, &imports)
            .map_err(Failure::from);
    }
    if cmd == "update" {
        let manifest = match &args[1..] {
            [] => package::discover_manifest()?,
            [input] => package::manifest_path(Path::new(input)).ok_or("expected a package directory or ae.toml")?,
            _ => return Err(Failure::usage(USAGE)),
        };
        package::update_lock(&manifest)?;
        return Ok(0);
    }
    if cmd == "deps" {
        let manifest = match &args[1..] {
            [] => Some(package::discover_manifest()?),
            [input] => package::manifest_path(Path::new(input)),
            _ => return Err(Failure::usage(USAGE)),
        };
        let description = match manifest {
            Some(manifest) => package::describe_graph(&manifest)?,
            None => frontend::describe_path(Path::new(&args[1]))?,
        };
        print!("{description}");
        return Ok(0);
    }
    // `check --json <file>` is the documented spelling; `check <file> --json`
    // is accepted too because both readings are natural and the flag has no
    // other position to be confused with.
    let json_check = match (cmd.as_str(), &args[1..]) {
        ("check", [flag]) if flag == "--json" => Some(package::discover_manifest()),
        ("check", [flag, input]) | ("check", [input, flag]) if flag == "--json" => {
            Some(Ok(PathBuf::from(input)))
        }
        _ => None,
    };
    if let Some(source) = json_check {
        // Editor diagnostics: one structured array on stdout, nothing else.
        // Success prints `[]`; lexical recovery may return multiple errors.
        let started = Instant::now();
        #[allow(clippy::result_large_err)]
        let report = match source.and_then(|path| package::entry_path(&path))
            .map_err(frontend::FrontendError::Legacy)
            .and_then(|entry| frontend::check_path_diagnostic_with_imports(&entry.path, &entry.imports)) {
            Ok(()) => "[]".to_owned(),
            Err(frontend::FrontendError::Legacy(message)) => format!("[{}]", diagnostic_json(&message)),
            Err(frontend::FrontendError::Diagnostic(diagnostic)) => format!("[{}]", structured_diagnostic_json(&diagnostic)),
            Err(frontend::FrontendError::Diagnostics(diagnostics) | frontend::FrontendError::LexDiagnostics(diagnostics)) => format!("[{}]", diagnostics.iter().map(structured_diagnostic_json).collect::<Vec<_>>().join(",")),
        };
        println!("{report}");
        if timings_enabled() {
            eprintln!("ae timing: frontend_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
        }
        return Ok(if report == "[]" { 0 } else { 1 });
    }
    // `check --render` spells both ways like `--json`. Diagnostics keep the
    // plain `ae: file:line:column: message` line and gain a source
    // snippet whose underline follows the display policy (ADR-0212), so the
    // terminal points at the same token an editor does. A diagnostic without
    // a drawable span (a legacy text message, an unreadable file, a foreign
    // path) prints without a snippet rather than inventing one.
    let render_check = match (cmd.as_str(), &args[1..]) {
        ("check", [flag]) if flag == "--render" => Some(package::discover_manifest()),
        ("check", [flag, input]) | ("check", [input, flag]) if flag == "--render" => {
            Some(Ok(PathBuf::from(input)))
        }
        _ => None,
    };
    if let Some(source) = render_check {
        #[allow(clippy::result_large_err)]
        let outcome = source.and_then(|path| package::entry_path(&path))
            .map_err(frontend::FrontendError::Legacy)
            .and_then(|entry| frontend::check_path_diagnostic_with_imports(&entry.path, &entry.imports));
        match outcome {
            Ok(()) => return Ok(0),
            Err(frontend::FrontendError::Legacy(message)) => {
                eprintln!("ae: {message}");
                return Ok(1);
            }
            Err(error) => {
                let diagnostics: Vec<frontend::FrontendDiagnostic> = match error {
                    frontend::FrontendError::Diagnostic(diagnostic) => vec![diagnostic],
                    frontend::FrontendError::Diagnostics(diagnostics)
                    | frontend::FrontendError::LexDiagnostics(diagnostics) => diagnostics,
                    frontend::FrontendError::Legacy(_) => unreachable!("handled above"),
                };
                let mut file_cache: std::collections::HashMap<&str, Option<String>> = std::collections::HashMap::new();
                for diagnostic in &diagnostics {
                    eprintln!("ae: {diagnostic}");
                    let Some(label) = &diagnostic.primary else { continue };
                    let span = &label.span;
                    let Some(file) = &span.file else { continue };
                    let text = file_cache
                        .entry(file.as_str())
                        .or_insert_with(|| fs::read_to_string(file).ok());
                    let Some(text) = text.as_deref() else { continue };
                    if let Some(snippet) = render::render_snippet(text, span.line, span.range.start, span.range.end) {
                        eprintln!("{snippet}");
                    }
                }
                return Ok(1);
            }
        }
    }
    let mut args = args;
    let first = args.get(1).and_then(|arg| arg.to_str());
    let implicit_project = match cmd.as_str() {
        "check" | "emit-c" | "emit-tree" | "emit-cfg" | "emit-mir" | "emit-resolve" | "emit-types" | "emit-hir" => args.len() == 1,
        "run" => args.len() == 1 || matches!(first, Some("--release" | "--debug" | "--interp" | "--")),
        "build" => first == Some("-o"),
        _ => false,
    };
    if implicit_project {
        args.insert(1, package::discover_manifest()?.into_os_string());
    }
    if args.len() < 2 { return Err(Failure::usage(USAGE)); }
    // `run` forwards everything after `--` to the program, so a script that
    // reads `args()` can be exercised through the toolchain instead of being
    // built and invoked by hand. The split happens before the flags are read,
    // so an optimization flag may sit on either side of the separator.
    let mut program_args: Vec<OsString> = Vec::new();
    if let Some(at) = args.iter().position(|a| a.to_str() == Some("--")) {
        program_args = args.split_off(at + 1);
        args.pop();
        if cmd != "run" { return Err(Failure::usage(format!("'{cmd}' does not take program arguments\n{USAGE}"))); }
    }
    // `build` writes the file a user ships, so it is optimized unless --debug
    // asks for the fast build; `run` is the development loop and stays at -O0
    // unless --release asks for -O2. Same program, same generated C: on the
    // bench workloads -O2 ran 1.34x (text) to 4.05x (sum) faster on Linux and
    // 1.29x (textlazy) to 1.66x (map) on Windows, costing the fresh-cache
    // build +13 to +87 ms on Linux / +41 to +191 ms on Windows on 14-27 KB of
    // generated C, with slightly smaller executables (ADR-59). `build
    // --release` is still accepted: the flag now names the default, and
    // existing scripts pass it.
    let interp_run = cmd == "run" && args.len() == 3 && args.last().and_then(|a| a.to_str()) == Some("--interp");
    if interp_run {
        let package::PackageEntry { path: input, imports, .. } = package::entry_path(Path::new(&args[1]))?;
        let input = input.as_path();
        match frontend::interpret_path_with_imports(input, &imports) {
            Ok((out, code)) => {
                print!("{out}");
                return Ok(code);
            }
            Err((out, msg)) => {
                print!("{out}");
                return Err(msg.into());
            }
        }
    }
    let level = match args.last().and_then(|a| a.to_str()) {
        Some("--release") => Some(true),
        Some("--debug") => Some(false),
        _ => None,
    };
    let release = match cmd.as_str() {
        "build" => level.unwrap_or(true),
        _ => level.unwrap_or(false),
    };
    let output = match cmd.as_str() {
        "check" | "emit-c" | "emit-tree" | "emit-cfg" | "emit-mir" | "emit-resolve" | "emit-types" | "emit-hir" | "run" if args.len() == 2 => None,
        "run" if level.is_some() => None,
        "build" if (args.len() == 4 || level.is_some()) && args[2] == "-o" => Some(PathBuf::from(&args[3])),
        "check" | "emit-c" | "emit-tree" | "emit-cfg" | "emit-mir" | "emit-resolve" | "emit-types" | "emit-hir" | "run" | "build" => return Err(Failure::usage(USAGE)),
        _ => return Err(Failure::usage(format!("unknown command: {cmd}\n{USAGE}"))),
    };
    let package::PackageEntry { path: input, imports, lock } = package::entry_path(Path::new(&args[1]))?;
    let import_key = sha256::hex(format!("{imports:?}").as_bytes());
    run_source(&cmd, &input, output, &program_args, release, &import_key, &imports, lock.filter(|_| cmd == "build"))
        .map_err(Failure::from)
}

/// Renders doc comments as Markdown: one section per block, headed by the
/// declaration the block sits above. Text is passed through unchanged so a
/// doc comment can use any Markdown the author wants.
fn doc_render(path: &Path, source: &str) -> Result<u8, Failure> {
    let blocks = ae_syntax::doc_blocks(source);
    if blocks.is_empty() {
        println!("{}: no doc comments", path.display());
        return Ok(0);
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    println!("# {name}\n");
    for block in blocks {
        if !block.declaration.is_empty() {
            println!("## `{}`\n", block.declaration);
        }
        println!("{}", block.text);
    }
    Ok(0)
}

/// Compiles and runs every fenced example in the file's doc comments. Each
/// example is a complete program; a failure names the file and the line of the
/// opening fence, not the generated temporary file.
fn doc_test(path: &Path, source: &str) -> Result<u8, Failure> {
    let examples = ae_syntax::doc_examples(source)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if examples.is_empty() {
        println!("{}: no doc examples", path.display());
        return Ok(0);
    }
    let mut failures = 0;
    for example in &examples {
        let directory = TempDir::create(&env::temp_dir())?;
        let file = directory.0.join("doc-example.ae");
        let outcome = fs::write(&file, &example.source)
            .map_err(|error| format!("{}: {error}", file.display()))
            .and_then(|()| frontend::compile_path(&file))
            .and_then(|c| build_c(&c, &directory.0, false, false))
            .and_then(|exe| Command::new(&exe).output()
                .map_err(|error| format!("cannot start doc example: {error}")));
        match outcome {
            Ok(output) if output.status.success() => {
                println!("ok: doc example at {}:{}", path.display(), example.line);
            }
            Ok(output) => {
                failures += 1;
                let code = output.status.code()
                    .map_or_else(|| "abnormally".to_owned(), |code| code.to_string());
                eprintln!("{}:{}: doc example exited with {}", path.display(), example.line, code);
            }
            Err(error) => {
                failures += 1;
                eprintln!("{}:{}: doc example failed: {error}", path.display(), example.line);
            }
        }
    }
    println!("{}: {} doc example(s), {} failed", path.display(), examples.len(), failures);
    Ok(if failures == 0 { 0 } else { 1 })
}

fn run_tests(input: &Path, filter: Option<&str>, use_interp: bool, imports: &frontend::PackageImports) -> Result<u8, String> {
    let started = Instant::now();
    // The staged copy is dropped (and deleted) on every path below.
    let program = harness::prepare(input, filter)?;
    if use_interp {
        match frontend::interpret_path_with_imports(&program.staging, imports) {
            Ok((out, code)) => {
                print!("{out}");
                if timings_enabled() {
                    eprintln!("ae timing: test_interp_ms={:.3} tests={}", started.elapsed().as_secs_f64() * 1000.0, program.tests.len());
                }
                return Ok(code);
            }
            Err((out, msg)) => {
                print!("{out}");
                return Err(msg);
            }
        }
    }
    let c = frontend::compile_path_with_imports(&program.staging, imports)?;
    let dir = TempDir::create(&env::temp_dir())?;
    let exe = build_c(&c, &dir.0, false, true)?;
    let status = Command::new(&exe).status().map_err(|e| format!("cannot start test program: {e}"))?;
    if timings_enabled() {
        eprintln!("ae timing: test_ms={:.3} tests={}", started.elapsed().as_secs_f64() * 1000.0, program.tests.len());
    }
    Ok(status.code().and_then(|n| u8::try_from(n).ok()).unwrap_or(1))
}

#[allow(clippy::too_many_arguments)]
fn run_source(
    cmd: &str,
    input: &Path,
    output: Option<PathBuf>,
    program_args: &[OsString],
    release: bool,
    import_key: &str,
    imports: &frontend::PackageImports,
    lock: Option<package::PackageLock>,
) -> Result<u8, String> {
    let started = Instant::now();
    if cmd == "emit-resolve" {
        // The resolution dump is its own pipeline stage over the shared
        // grammar (ADR-0224 R3b): it never touches the frontend or its
        // cache, so a fresh pass is the only honest source. Single-file
        // resolution for now — `use` headers are listed, not followed.
        // `use` headers are followed (transitively, cycle-guarded): the
        // imports' `pub` items become visible, so a program's names resolve
        // the way the compiler sees them.
        let (module, resolution) = ae_resolve::load_and_resolve(input).map_err(|e| format!("{}: {e}", input.display()))?;
        print!("{}", ae_resolve::dump(&module, &resolution));
        return Ok(0);
    }
    if cmd == "emit-types" {
        // The type facts are the resolver's own stage output (P1.2): a
        // deterministic dump of signatures, spellings, local shapes and
        // checked scalar results, pinned by tests/types snapshots.
        let (module, resolution) = ae_resolve::load_and_resolve(input).map_err(|e| format!("{}: {e}", input.display()))?;
        print!("{}", ae_resolve::dump_types::dump(&module, &resolution));
        return Ok(0);
    }
    if cmd == "emit-hir" {
        // The HIR view is built from the shared syntax tree plus the
        // resolver's def ids and parsed closure bodies (P1.3 slice 1).
        let (module, resolution) = ae_resolve::load_and_resolve(input).map_err(|e| format!("{}: {e}", input.display()))?;
        let hir = ae_hir::build(&module, Some(&resolution));
        print!("{}", ae_hir::dump::dump(&hir));
        return Ok(0);
    }
    if cmd == "emit-mir" {
        let mir = frontend::mir_path_with_imports(input, imports)?;
        print!("{mir}");
        return Ok(0);
    }
    if cmd == "emit-cfg" {
        let graph = frontend::cfg_path_with_imports(input, imports)?;
        print!("{graph}");
        return Ok(0);
    }
    if cmd == "emit-tree" {
        // The tree always comes from a fresh frontend pass: like `emit-c`,
        // its output IS the frontend's answer, so the reuse cache never
        // stands in for it.
        let tree = frontend::tree_path_with_imports(input, imports)?;
        print!("{tree}");
        return Ok(0);
    }
    if cmd == "check" {
        frontend::check_path_with_imports(input, imports)?;
        if timings_enabled() {
            eprintln!("ae timing: frontend_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
        }
        return Ok(0);
    }
    // `build` and `run` only need the C; if no source byte moved since the
    // previous build, the frontend work is skipped entirely. `emit-c` and
    // `check` always run the frontend, because their output *is* the
    // frontend's answer.
    let cacheable = matches!(cmd, "build" | "run");
    let c = match if cacheable { frontend_cache::lookup(&cache_root(), input, import_key) } else { None } {
        Some(hit) => {
            if let Some(lock) = &lock { lock.verify(&hit.units)?; }
            if timings_enabled() {
                eprintln!("ae frontend-cache: hit (key {})", hit.key);
                eprintln!("ae timing: frontend_and_codegen_ms=0.000 (frontend cache hit)");
            }
            hit.c
        }
        None => {
            let tracked = frontend::compile_path_tracked_with_imports(input, imports)?;
            if let Some(lock) = &lock { lock.verify(&tracked.units)?; }
            let c = format!("{}{}", tracked.runtime, tracked.user);
            let key = frontend_cache::store_c(&cache_root(), input, import_key, &tracked, &c);
            if timings_enabled() {
                if cacheable {
                    eprintln!("ae frontend-cache: miss (key {})", key.unwrap_or_else(|| "not stored".into()));
                } else {
                    eprintln!("ae frontend-cache: bypassed (this command always compiles)");
                }
                eprintln!("ae timing: frontend_and_codegen_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
            }
            c
        }
    };
    match cmd {
        "emit-c" => { print!("{c}"); Ok(0) },
        "run" => {
            let dir = TempDir::create(&env::temp_dir())?;
            let exe = build_c(&c, &dir.0, release, true)?;
            let status = Command::new(&exe).args(program_args).status().map_err(|e| format!("cannot start program: {e}"))?;
            Ok(status.code().and_then(|n| u8::try_from(n).ok()).unwrap_or(1))
        }
        "build" => {
            let output = output.unwrap();
            // A build never overwrites user files, including the input. The final copy uses
            // create_new as a second check so an intervening file is not overwritten either.
            let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let dir = TempDir::create(parent)?;
            let exe = build_c(&c, &dir.0, release, true)?;
            let mut src = fs::File::open(&exe).map_err(|e| format!("opening generated executable: {e}"))?;
            let mut dest = fs::OpenOptions::new().write(true).create_new(true).open(&output)
                .map_err(|e| format!("cannot create {} (existing files are not overwritten): {e}", output.display()))?;
            if let Err(e) = std::io::copy(&mut src, &mut dest) {
                drop(dest); let _ = fs::remove_file(&output);
                return Err(format!("writing {}: {e}", output.display()));
            }
            drop(dest);
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = fs::set_permissions(&output, fs::Permissions::from_mode(0o755)) {
                    let _ = fs::remove_file(&output);
                    return Err(format!("setting executable permissions on {}: {e}", output.display()));
                }
            }
            Ok(0)
        }
        _ => unreachable!(),
    }

}

fn main() -> ExitCode {
    // Parsing, checking, and dropping deep ASTs recurse; give that work a
    // dedicated large stack instead of relying on the OS default for the
    // main thread. Nesting limits still bound recursion explicitly.
    let worker = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| run(env::args_os().skip(1).collect()));
    let result = match worker {
        Ok(worker) => match worker.join() {
            Ok(code) => code,
            Err(_) => return ExitCode::FAILURE,
        },
        Err(e) => Err(Failure::from(format!("cannot start compiler thread: {e}"))),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            // A runtime trap carries its own `ae:` prefix (the exact
            // format the compiled binary's runtime prints); diagnostics get
            // the prefix here.
            if failure.message.starts_with("ae:") { eprintln!("{}", failure.message); }
            else { eprintln!("ae: {}", failure.message); }
            ExitCode::from(failure.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_split_into_structured_parts() {
        let json = diagnostic_json("src/main.ae:3:14: unknown variable 'x'");
        assert_eq!(json, "{\"file\":\"src/main.ae\",\"line\":3,\"column\":14,\"message\":\"unknown variable 'x'\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}");
        // No file name (in-memory sources).
        let json = diagnostic_json("2:5: assertion failed");
        assert_eq!(json, "{\"file\":null,\"line\":2,\"column\":5,\"message\":\"assertion failed\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}");
    }

    #[test]
    fn non_location_messages_survive_whole() {
        // "program must define exactly one main function (found 0)" has a
        // colon but no numbers; a foreign message has no colon at all.
        let json = diagnostic_json("program must define exactly one main function (found 0)");
        assert!(json.contains("\"file\":null") && json.contains("\"line\":null"), "{json}");
        assert!(json.contains("(found 0)"), "{json}");
        let json = diagnostic_json("no colons at all");
        assert!(json.contains("\"message\":\"no colons at all\""), "{json}");
    }

    #[test]
    fn windows_paths_and_quotes_are_escaped() {
        // Backslashes (Windows paths), quotes and a non-ASCII name all pass
        // through the escaper unchanged in meaning.
        let json = diagnostic_json("D:\\用户\\trevv\\main.ae:1:1: cannot read \"input\": missing");
        assert_eq!(json,
            "{\"file\":\"D:\\\\用户\\\\trevv\\\\main.ae\",\"line\":1,\"column\":1,\"message\":\"cannot read \\\"input\\\": missing\",\"byte_start\":null,\"byte_end\":null,\"code\":null,\"suggestions\":[],\"secondary\":[]}");
    }
}
