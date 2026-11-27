use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "ae.toml";

const PACKAGE_NAME_ERROR: &str = "package.name must have 1 to 64 lowercase letters, digits, underscores or hyphens";

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    })
}

const HELLO: &str = "fn main() {\n    print(\"Hello, ae!\");\n}\n";

fn manifest_text(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nae = \"{}\"\nedition = \"2\"\n",
        env!("CARGO_PKG_VERSION"),
    )
}

fn create_file(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = fs::File::create_new(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(content.as_bytes()).map_err(|error| format!("{}: {error}", path.display()))
}

pub(super) fn create_project(directory: &Path) -> Result<(), String> {
    let name = directory.file_name().and_then(|name| name.to_str())
        .filter(|name| valid_name(name)).ok_or(PACKAGE_NAME_ERROR)?;
    fs::create_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let source = directory.join("src");
    fs::create_dir(&source).map_err(|error| format!("{}: {error}", source.display()))?;
    create_file(&source.join("main.ae"), HELLO)?;
    create_file(&directory.join(MANIFEST), &manifest_text(name))
}

/// Scaffolds a package inside an existing directory. A manifest already present is an
/// error rather than an overwrite; an existing `src/main.ae` is left untouched, so `init`
/// on a directory that already holds sources only adds the manifest.
pub(super) fn init_project(directory: &Path) -> Result<(), String> {
    if !directory.is_dir() {
        return Err(format!("{}: init needs an existing directory; use ae new for a new one", directory.display()));
    }
    let canonical = directory.canonicalize().map_err(|error| format!("{}: {error}", directory.display()))?;
    let name = canonical.file_name().and_then(|name| name.to_str())
        .filter(|name| valid_name(name)).ok_or(PACKAGE_NAME_ERROR)?;
    let manifest = directory.join(MANIFEST);
    if manifest.try_exists().map_err(|error| format!("{}: {error}", manifest.display()))? {
        return Err(format!("{}: already contains {MANIFEST}", directory.display()));
    }
    let source = directory.join("src");
    fs::create_dir_all(&source).map_err(|error| format!("{}: {error}", source.display()))?;
    let main = source.join("main.ae");
    if !main.try_exists().map_err(|error| format!("{}: {error}", main.display()))? {
        create_file(&main, HELLO)?;
    }
    fs::write(&manifest, manifest_text(name)).map_err(|error| format!("{}: {error}", manifest.display()))
}

pub(super) fn discover_manifest() -> Result<PathBuf, String> {
    let current = std::env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    for directory in current.ancestors() {
        let manifest = directory.join(MANIFEST);
        if manifest.try_exists().map_err(|error| format!("{}: {error}", manifest.display()))? {
            return Ok(manifest);
        }
    }
    Err(format!("no {MANIFEST} found in {} or its parent directories", current.display()))
}

pub(super) fn manifest_path(input: &Path) -> Option<PathBuf> {
    if input.is_dir() {
        Some(input.join(MANIFEST))
    } else if input.file_name().is_some_and(|name| name == MANIFEST) {
        Some(input.to_owned())
    } else {
        None
    }
}

pub(super) struct PackageEntry {
    pub path: PathBuf,
    pub imports: ae_compiler::frontend::PackageImports,
    pub lock: Option<PackageLock>,
}

pub(super) fn entry_path(input: &Path) -> Result<PackageEntry, String> {
    let Some(manifest) = manifest_path(input) else {
        return Ok(PackageEntry { path: input.to_owned(), imports: Default::default(), lock: None });
    };
    let package = read_manifest(&manifest)?;
    let lock_path = manifest.with_file_name("ae.lock");
    let mut graph = PathGraph {
        lock_path: Some(lock_path.clone()),
        ..PathGraph::default()
    };
    // The root package's edition is registered even when the graph is not
    // walked (no dependencies, no lock): the entry file still belongs to it.
    let canonical_manifest = manifest.canonicalize().unwrap_or_else(|_| manifest.clone());
    graph.imports.set_edition(canonical_manifest.with_file_name("src"), package.edition);
    let has_lock = lock_path.try_exists().map_err(|error| format!("{}: {error}", lock_path.display()))?;
    let lock = if package.dependencies.is_empty() && !has_lock {
        None
    } else {
        graph.visit(&manifest)?;
        Some(PackageLock {
            path: lock_path,
            manifests: graph.manifests,
            archive_lines: graph.archive_lines,
            extracted_roots: graph.extracted_roots,
        })
    };
    Ok(PackageEntry { path: manifest.with_file_name("src").join("main.ae"), imports: graph.imports, lock })
}

pub(super) struct PackageLock {
    path: PathBuf,
    manifests: Vec<(PathBuf, String)>,
    archive_lines: Vec<String>,
    extracted_roots: Vec<PathBuf>,
}

impl PackageLock {
    fn render(&self, units: &[ae_compiler::frontend::SourceUnit]) -> Result<String, String> {
        use std::fmt::Write;
        let base = self.path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let base = base.canonicalize().map_err(|error| format!("{}: {error}", base.display()))?;
        let local = |path: &Path| -> Result<String, String> {
            let from: Vec<_> = base.components().collect();
            let to: Vec<_> = path.components().collect();
            let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
            if shared == 0 {
                return Err(format!("cannot lock a file on another filesystem root: {}", path.display()));
            }
            let mut parts = vec![".."; from.len() - shared];
            for component in &to[shared..] {
                parts.push(component.as_os_str().to_str().ok_or_else(|| format!(
                    "cannot lock a non-UTF-8 path: {}", path.display()
                ))?);
            }
            Ok(parts.join("/"))
        };

        let mut text = String::from("ae-lock-v1\n");
        let mut archives = self.archive_lines.clone();
        archives.sort();
        archives.dedup();
        for line in archives {
            writeln!(text, "{line}")
                .map_err(|error| format!("failed to format lockfile: {error}"))?;
        }
        for (path, digest) in &self.manifests {
            writeln!(text, "manifest {digest} {:?}", local(path)?)
                .map_err(|error| format!("failed to format lockfile: {error}"))?;
        }
        let mut sources = Vec::new();
        for unit in units {
            let path = unit.path.canonicalize().map_err(|error| format!("{}: {error}", unit.path.display()))?;
            if self.extracted_roots.iter().any(|root| path.starts_with(root)) {
                continue;
            }
            sources.push((local(&path)?, &unit.digest));
        }
        sources.sort();
        for (path, digest) in sources {
            writeln!(text, "source {digest} {path:?}")
                .map_err(|error| format!("failed to format lockfile: {error}"))?;
        }
        Ok(text)
    }

    pub fn verify(&self, units: &[ae_compiler::frontend::SourceUnit]) -> Result<(), String> {
        let recorded = fs::read_to_string(&self.path)
            .map_err(|error| format!("{}: {error}. Run ae update", self.path.display()))?;
        let actual = self.render(units)?;
        if recorded.lines().eq(actual.lines()) {
            return Ok(());
        }
        let mut locked_lines = recorded.lines();
        let mut actual_lines = actual.lines();
        let (expected, found) = loop {
            let locked = locked_lines.next();
            let observed = actual_lines.next();
            if locked != observed {
                break (locked.unwrap_or("<missing entry>"), observed.unwrap_or("<missing entry>"));
            }
        };
        Err(format!("{} is stale. Run ae update\n  locked: {expected}\n  actual: {found}", self.path.display()))
    }
}

pub(super) fn update_lock(manifest: &Path) -> Result<(), String> {
    let PackageEntry { path, imports, lock } = entry_path(manifest)?;
    let lock = match lock {
        Some(lock) => lock,
        None => {
            let canonical = manifest.canonicalize().map_err(|error| format!("{}: {error}", manifest.display()))?;
            let package = read_manifest(&canonical)?;
            PackageLock {
                path: manifest.with_file_name("ae.lock"),
                manifests: vec![(canonical, package.digest)],
                archive_lines: Vec::new(),
                extracted_roots: Vec::new(),
            }
        }
    };
    let tracked = ae_compiler::frontend::compile_path_tracked_with_imports(&path, &imports)?;
    let contents = lock.render(&tracked.units)?;
    let parent = lock.path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let temporary = super::TempDir::create(parent)?;
    let staged = temporary.0.join("ae.lock");
    fs::write(&staged, contents).map_err(|error| format!("{}: {error}", staged.display()))?;
    fs::rename(&staged, &lock.path).map_err(|error| format!("{}: {error}", lock.path.display()))
}

struct Manifest {
    name: String,
    version: Version,
    /// The package's source edition: 1 for a manifest that predates the
    /// field, 2 for the current edition (`ae new`/`ae init` scaffold it).
    edition: u32,
    dependencies: toml::Table,
    digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(text: &str, allow_missing_patch: bool) -> Option<Self> {
        let (major, minor, patch) = parse_version(text, allow_missing_patch)?;
        Some(Self { major, minor, patch })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// An MVS-compatible caret requirement: 0.3 admits 0.3.x but not 0.4.0.
struct Requirement {
    text: String,
    minimum: Version,
    ceiling: Version,
}

impl Requirement {
    fn parse(text: &str) -> Option<Self> {
        let minimum = Version::parse(text, true)?;
        let ceiling = if minimum.major > 0 {
            Version { major: minimum.major + 1, minor: 0, patch: 0 }
        } else if minimum.minor > 0 {
            Version { major: 0, minor: minimum.minor + 1, patch: 0 }
        } else {
            Version { major: 0, minor: 0, patch: minimum.patch + 1 }
        };
        Some(Self { text: text.to_owned(), minimum, ceiling })
    }

    fn contains(&self, version: Version) -> bool {
        version >= self.minimum && version < self.ceiling
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

fn read_manifest(manifest: &Path) -> Result<Manifest, String> {
    let source = fs::read_to_string(manifest)
        .map_err(|error| format!("{}: {error}", manifest.display()))?;
    parse_manifest_str(manifest, &source)
}

fn parse_manifest_str(manifest: &Path, source: &str) -> Result<Manifest, String> {
    let fail = |message: &str| format!("{}: {message}", manifest.display());
    let mut document = source.parse::<toml::Table>().map_err(|error| fail(&error.to_string()))?;
    let package = document.get("package").and_then(toml::Value::as_table)
        .ok_or_else(|| fail("expected a [package] table"))?;
    let field = |key| package.get(key).and_then(toml::Value::as_str)
        .ok_or_else(|| fail(&format!("package.{key} must be a string")));

    let name = field("name")?;
    if !valid_name(name) {
        return Err(fail(PACKAGE_NAME_ERROR));
    }

    let version = Version::parse(field("version")?, false)
        .ok_or_else(|| fail("package.version must be a three-component version such as 0.1.0"))?;
    let floor = field("ae")?;
    let minimum = Version::parse(floor, true).ok_or_else(|| {
        fail("package.ae must be a toolchain version such as 0.1 or 0.1.1")
    })?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"), false)
        .ok_or_else(|| fail("invalid compiler release version"))?;
    if minimum > current {
        return Err(fail(&format!("package requires ae {floor} or later, installed version is {current}")));
    }
    // M8.1.02: the edition is optional so every pre-edition manifest keeps
    // edition 1; only the two published editions are accepted.
    let edition = match package.get("edition") {
        None => 1,
        Some(toml::Value::String(text)) => match text.as_str() {
            "1" => 1,
            "2" => 2,
            other => return Err(fail(&format!("package.edition must be \"1\" or \"2\", found \"{other}\""))),
        },
        Some(_) => return Err(fail("package.edition must be a string")),
    };

    let name = name.to_owned();
    let mut dependencies = match document.remove("dependencies") {
        None => toml::Table::new(),
        Some(toml::Value::Table(table)) => table,
        Some(_) => return Err(fail("expected a [dependencies] table")),
    };
    if let Some(targets) = document.remove("target") {
        let targets = targets.as_table().ok_or_else(|| fail("expected a [target] table"))?;
        for (selector, value) in targets {
            let condition = selector.strip_prefix("cfg(").and_then(|prefix| prefix.strip_suffix(')'))
                .and_then(platform_matches)
                .ok_or_else(|| fail(&format!("unsupported target selector '{selector}'")))?;
            let table = value.as_table().filter(|table| table.len() == 1)
                .and_then(|table| table.get("dependencies")).and_then(toml::Value::as_table)
                .ok_or_else(|| fail(&format!("target '{selector}' requires a dependencies table")))?;
            if condition {
                for (alias, dependency) in table {
                    if let Some(previous) = dependencies.get(alias) {
                        if previous != dependency {
                            return Err(fail(&format!("multiple active declarations for dependency '{alias}'")));
                        }
                    } else {
                        dependencies.insert(alias.clone(), dependency.clone());
                    }
                }
            }
        }
    }
    Ok(Manifest { name, version, edition, dependencies, digest: ae_compiler::sha256::hex(source.as_bytes()) })
}

fn platform_matches(platform: &str) -> Option<bool> {
    Some(match platform {
        "windows" => cfg!(windows),
        "unix" => cfg!(unix),
        "x86_64" => cfg!(target_arch = "x86_64"),
        "arm64" => cfg!(target_arch = "aarch64"),
        _ => return None,
    })
}

const ARCHIVE_MAGIC: &[u8; 8] = b"AEAR1\0\0\0";

fn is_valid_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Verifies a cached package archive's SHA-256 checksum against the expected
/// hex digest from `ae.lock` before any extraction or compilation begins.
/// Fails closed with an error naming the package, archive path, expected hash,
/// and observed hash.
pub(super) fn verify_cached_archive(
    package: &str,
    archive_path: &Path,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    if !is_valid_sha256_hex(expected_sha256) {
        let expected_display = if expected_sha256.is_empty() { "<missing>" } else { expected_sha256 };
        return Err(format!(
            "cached package '{package}' archive {} has missing or malformed checksum: expected {expected_display}, observed <not verified>",
            archive_path.display()
        ));
    }
    let expected = expected_sha256.to_ascii_lowercase();
    let bytes = fs::read(archive_path).map_err(|error| {
        format!(
            "cached package '{package}' archive missing or unreadable at {}: expected {expected}, observed <missing> ({error})",
            archive_path.display()
        )
    })?;
    let observed = ae_compiler::sha256::hex(&bytes);
    if observed != expected {
        return Err(format!(
            "cached package '{package}' archive checksum mismatch at {}: expected {expected}, observed {observed}",
            archive_path.display()
        ));
    }
    Ok(bytes)
}

fn validate_archive_entry_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("archive contains an empty entry path".to_owned());
    }
    if raw.starts_with('/') || raw.starts_with('\\') || raw.contains('\\') {
        return Err(format!("archive entry path '{raw}' is unsafe"));
    }
    let mut normalized = PathBuf::new();
    for segment in raw.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("archive entry path '{raw}' escapes extraction root"));
        }
        if segment.contains(':') {
            return Err(format!("archive entry path '{raw}' contains a drive prefix"));
        }
        normalized.push(segment);
    }
    Ok(normalized)
}

/// Packs `(relative_path, bytes)` entries into a deterministic `AEAR1` archive.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn pack_archive(entries: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut sorted = Vec::with_capacity(entries.len());
    let mut seen = std::collections::HashSet::new();
    for &(path, data) in entries {
        validate_archive_entry_path(path)?;
        if !seen.insert(path) {
            return Err(format!("duplicate archive entry '{path}'"));
        }
        sorted.push((path, data));
    }
    sorted.sort_by_key(|(a, _)| *a);

    let count = u32::try_from(sorted.len()).map_err(|_| "archive has too many entries".to_owned())?;
    let mut out = Vec::new();
    out.extend_from_slice(ARCHIVE_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    for (path, data) in sorted {
        let path_bytes = path.as_bytes();
        let path_len = u32::try_from(path_bytes.len())
            .map_err(|_| format!("archive entry path too long: '{path}'"))?;
        let data_len = u64::try_from(data.len())
            .map_err(|_| format!("archive entry too large: '{path}'"))?;
        out.extend_from_slice(&path_len.to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(data);
    }
    Ok(out)
}

fn parse_archive_entries<'a>(package: &str, archive_path: &Path, bytes: &'a [u8]) -> Result<Vec<(PathBuf, &'a [u8])>, String> {
    if bytes.len() < ARCHIVE_MAGIC.len() + 4 || &bytes[..ARCHIVE_MAGIC.len()] != ARCHIVE_MAGIC {
        return Err(format!(
            "cached package '{package}' archive {} is invalid or truncated",
            archive_path.display()
        ));
    }
    let mut offset = ARCHIVE_MAGIC.len();
    let count = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes")) as usize;
    offset += 4;

    let mut entries = Vec::with_capacity(count.min(1024));
    let mut seen = std::collections::HashSet::new();
    for _ in 0..count {
        if offset + 4 > bytes.len() {
            return Err(format!(
                "cached package '{package}' archive {} is truncated (missing entry path length)",
                archive_path.display()
            ));
        }
        let path_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes")) as usize;
        offset += 4;
        if offset + path_len > bytes.len() {
            return Err(format!(
                "cached package '{package}' archive {} is truncated (missing entry path)",
                archive_path.display()
            ));
        }
        let raw_path = std::str::from_utf8(&bytes[offset..offset + path_len]).map_err(|_| {
            format!("cached package '{package}' archive {} contains non-UTF-8 entry path", archive_path.display())
        })?;
        offset += path_len;
        let rel_path = validate_archive_entry_path(raw_path)?;
        if !seen.insert(rel_path.clone()) {
            return Err(format!(
                "cached package '{package}' archive {} contains duplicate entry '{raw_path}'",
                archive_path.display()
            ));
        }
        if offset + 8 > bytes.len() {
            return Err(format!(
                "cached package '{package}' archive {} is truncated (missing entry size)",
                archive_path.display()
            ));
        }
        let data_len_u64 = u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"));
        let data_len = usize::try_from(data_len_u64).map_err(|_| {
            format!("cached package '{package}' archive {} entry '{raw_path}' exceeds address space", archive_path.display())
        })?;
        offset += 8;
        if offset + data_len > bytes.len() {
            return Err(format!(
                "cached package '{package}' archive {} is truncated (missing entry contents for '{raw_path}')",
                archive_path.display()
            ));
        }
        let data = &bytes[offset..offset + data_len];
        offset += data_len;
        entries.push((rel_path, data));
    }
    if offset != bytes.len() {
        return Err(format!(
            "cached package '{package}' archive {} has unexpected trailing bytes",
            archive_path.display()
        ));
    }
    Ok(entries)
}

fn write_archive_entries_atomically(dest_dir: &Path, entries: &[(PathBuf, &[u8])]) -> Result<(), String> {
    let parent = dest_dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let staging = super::TempDir::create(parent)?;
    for (rel_path, data) in entries {
        let target = staging.0.join(rel_path);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        fs::write(&target, data).map_err(|error| format!("{}: {error}", target.display()))?;
    }
    match fs::rename(&staging.0, dest_dir) {
        Ok(()) => Ok(()),
        Err(_) if dest_dir.is_dir() => Ok(()),
        Err(error) => Err(format!("{}: {error}", dest_dir.display())),
    }
}

/// Verifies `archive_path` against `expected_sha256` and extracts its contents
/// into `dest_dir` only after verification and entry path validation succeed.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn extract_verified_archive(
    package: &str,
    archive_path: &Path,
    expected_sha256: &str,
    dest_dir: &Path,
) -> Result<(), String> {
    let bytes = verify_cached_archive(package, archive_path, expected_sha256)?;
    let entries = parse_archive_entries(package, archive_path, &bytes)?;
    write_archive_entries_atomically(dest_dir, &entries)
}

#[derive(Clone, Debug)]
struct ArchiveLockEntry {
    name: String,
    version: Version,
    sha256: String,
    archive_path: Option<String>,
    raw_line: String,
}

fn unquote_lock_path(token: &str) -> Option<String> {
    let inner = token.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next()? {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                other => {
                    let _ = other;
                    return None;
                }
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

fn parse_lock_archives(lock_path: &Path) -> Result<Vec<ArchiveLockEntry>, String> {
    let text = fs::read_to_string(lock_path).map_err(|error| {
        format!("{}: cannot read lockfile ({error}); run ae update", lock_path.display())
    })?;
    let mut lines = text.lines();
    match lines.next() {
        Some("ae-lock-v1") => {}
        _ => return Err(format!("{}: invalid lockfile header (expected ae-lock-v1)", lock_path.display())),
    }
    let mut entries = Vec::new();
    for line in lines {
        let Some(rest) = line.strip_prefix("archive ") else { continue; };
        let mut parts = rest.splitn(4, ' ');
        let name = parts.next().unwrap_or("");
        let version_str = parts.next().unwrap_or("");
        let sha_str = parts.next().unwrap_or("");
        let path_part = parts.next().map(str::trim).filter(|s| !s.is_empty());
        if name.is_empty() || version_str.is_empty() {
            return Err(format!("{}: malformed archive lock entry '{line}'", lock_path.display()));
        }
        let archive_path = match path_part {
            Some(quoted) => Some(unquote_lock_path(quoted).ok_or_else(|| {
                format!("{}: malformed archive path in lock entry for '{name}'", lock_path.display())
            })?),
            None => None,
        };
        let display_archive = archive_path.as_deref().unwrap_or("<cache>");
        if !is_valid_sha256_hex(sha_str) {
            let expected_display = if sha_str.is_empty() { "<missing>" } else { sha_str };
            return Err(format!(
                "cached package '{name}' archive {display_archive} has missing or malformed checksum in {}: expected {expected_display}, observed <not verified>",
                lock_path.display()
            ));
        }
        let version = Version::parse(version_str, false).ok_or_else(|| {
            format!(
                "cached package '{name}' archive {display_archive} has invalid version '{version_str}' in {}",
                lock_path.display()
            )
        })?;
        entries.push(ArchiveLockEntry {
            name: name.to_owned(),
            version,
            sha256: sha_str.to_ascii_lowercase(),
            archive_path,
            raw_line: line.to_owned(),
        });
    }
    Ok(entries)
}

pub(super) fn describe_graph(root: &Path) -> Result<String, String> {
    let mut graph = PathGraph {
        lock_path: Some(root.with_file_name("ae.lock")),
        ..PathGraph::default()
    };
    graph.visit(root)?;
    Ok(graph.output)
}

#[derive(Default)]
struct PathGraph {
    visiting: Vec<PathBuf>,
    visited: std::collections::HashMap<PathBuf, Version>,
    packages: std::collections::HashMap<String, (Version, PathBuf)>,
    output: String,
    imports: ae_compiler::frontend::PackageImports,
    manifests: Vec<(PathBuf, String)>,
    lock_path: Option<PathBuf>,
    lock_archives: Option<Vec<ArchiveLockEntry>>,
    archive_lines: Vec<String>,
    extracted_roots: Vec<PathBuf>,
}

impl PathGraph {
    fn visit(&mut self, manifest: &Path) -> Result<Version, String> {
        self.visit_inner(manifest, false)
    }

    fn resolve_archive_entry(&mut self, alias: &str) -> Result<ArchiveLockEntry, String> {
        let lock_path = self.lock_path.clone().ok_or_else(|| {
            format!("cached package '{alias}' requires ae.lock with a locked sha256 checksum (expected <missing>, observed <not verified>)")
        })?;
        if self.lock_archives.is_none() {
            let parsed = parse_lock_archives(&lock_path)?;
            self.lock_archives = Some(parsed);
        }
        let archives = self.lock_archives.as_ref().expect("populated");
        archives.iter().find(|entry| entry.name == alias).cloned().ok_or_else(|| {
            format!(
                "cached package '{alias}' has no locked archive entry in {}: expected <missing>, observed <not verified>",
                lock_path.display()
            )
        })
    }

    fn visit_inner(&mut self, manifest: &Path, is_cached_archive: bool) -> Result<Version, String> {
        use std::fmt::Write;
        let path = manifest.canonicalize().map_err(|error| format!("{}: {error}", manifest.display()))?;
        if let Some(start) = self.visiting.iter().position(|ancestor| ancestor == &path) {
            let cycle = self.visiting[start..].iter().chain(std::iter::once(&path))
                .map(|member| member.display().to_string()).collect::<Vec<_>>().join(" -> ");
            return Err(format!("package dependency cycle: {cycle}"));
        }
        if let Some(version) = self.visited.get(&path) {
            return Ok(*version);
        }
        if self.visiting.len() == 128 {
            return Err(format!("{}: package dependency nesting exceeds 128 levels", manifest.display()));
        }

        let package = read_manifest(&path)?;
        // Two copies of one package at different versions cannot both be linked;
        // name the two manifests instead of letting the second silently shadow the first.
        if let Some((known, origin)) = self.packages.get(&package.name) {
            if *known != package.version {
                return Err(format!(
                    "{}: package '{}' is version {} here but version {} in {}",
                    path.display(), package.name, package.version, known, origin.display()));
            }
        } else {
            self.packages.insert(package.name.clone(), (package.version, path.clone()));
        }
        // M8.1.02: the edition follows the declaring package, not its
        // consumer; a new-edition package may depend on an old-edition one.
        self.imports.set_edition(path.with_file_name("src"), package.edition);
        self.visiting.push(path.clone());
        let mut edges = Vec::new();
        let mut imports = std::collections::BTreeMap::new();
        for (alias, dependency) in package.dependencies {
            let table = dependency.as_table().ok_or_else(|| {
                format!("{}: dependency resolution for '{alias}' requires a nonempty path table", path.display())
            })?;
            let mut relative = None;
            let mut archive_override = None;
            let mut requirement = None;
            for (key, value) in table {
                match key.as_str() {
                    "path" => relative = value.as_str(),
                    "archive" => {
                        archive_override = Some(value.as_str().ok_or_else(|| {
                            format!("{}: dependency '{alias}' archive must be a string", path.display())
                        })?);
                    }
                    "version" => {
                        let text = value.as_str().ok_or_else(|| format!("{}: dependency '{alias}' version must be a string", path.display()))?;
                        requirement = Some(Requirement::parse(text).ok_or_else(|| format!("{}: dependency '{alias}' version '{text}' must be a major.minor version such as 0.3", path.display()))?);
                    }
                    other => return Err(format!("{}: dependency '{alias}' has unsupported key '{other}'", path.display())),
                }
            }
            if relative.is_some() && archive_override.is_some() {
                return Err(format!("{}: dependency '{alias}' cannot specify both 'path' and 'archive'", path.display()));
            }
            if relative.is_none() && (requirement.is_some() || archive_override.is_some()) {
                let locked = self.resolve_archive_entry(&alias)?;
                if let Some(requirement) = &requirement
                    && !requirement.contains(locked.version)
                {
                    return Err(format!(
                        "{}: dependency '{alias}' requires version {requirement} but locked archive is {}",
                        path.display(), locked.version
                    ));
                }
                let lock_base = self.lock_path.as_deref()
                    .and_then(Path::parent)
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let archive_path = match archive_override.or(locked.archive_path.as_deref()) {
                    Some(raw) => resolve_recorded_path(lock_base, raw),
                    None => super::cache_root()
                        .join("packages")
                        .join(format!("{}-{}.aepkg", locked.name, locked.version)),
                };
                // Fail-closed SHA-256 verification BEFORE any extraction or compilation.
                let verified_bytes = verify_cached_archive(&alias, &archive_path, &locked.sha256)?;
                let extract_dir = super::cache_root()
                    .join("extracted")
                    .join(format!("{}-{}-{}", locked.name, locked.version, locked.sha256));
                if !extract_dir.join(MANIFEST).is_file() {
                    let entries = parse_archive_entries(&alias, &archive_path, &verified_bytes)?;
                    write_archive_entries_atomically(&extract_dir, &entries)?;
                }
                let canonical_extract = extract_dir.canonicalize()
                    .map_err(|error| format!("{}: {error}", extract_dir.display()))?;
                self.extracted_roots.push(canonical_extract.clone());
                self.archive_lines.push(locked.raw_line.clone());

                let extracted_manifest = canonical_extract.join(MANIFEST);
                self.visit_inner(&extracted_manifest, true)?;
                imports.insert(alias.clone(), canonical_extract.join("src").join("lib.ae"));
                edges.push((alias, format!("archive:{}", archive_path.display())));
                continue;
            }
            let relative = relative.filter(|value| !value.is_empty()).ok_or_else(|| {
                format!("{}: dependency resolution for '{alias}' requires a nonempty path table", path.display())
            })?;
            let source = Path::new(relative);
            if source.has_root() || matches!(source.components().next(), Some(std::path::Component::Prefix(_))) {
                return Err(format!("{}: dependency '{alias}' path must be relative to its manifest", path.display()));
            }
            let dependency = path.parent()
                .ok_or_else(|| format!("{}: manifest path has no parent directory", path.display()))?
                .join(source).join(MANIFEST);
            let canonical = dependency.canonicalize().map_err(|error| format!("{}: {error}", dependency.display()))?;
            let version = self.visit_inner(&canonical, is_cached_archive)?;
            if let Some(requirement) = requirement
                && !requirement.contains(version)
            {
                return Err(format!("{}: dependency '{alias}' requires version {requirement} but {} is {version}",
                    path.display(), canonical.display()));
            }
            imports.insert(alias.clone(), canonical.with_file_name("src").join("lib.ae"));
            edges.push((alias, relative.to_owned()));
        }
        self.visiting.pop();
        self.imports.insert(path.with_file_name("src"), imports);
        self.visited.insert(path.clone(), package.version);
        if !is_cached_archive {
            self.manifests.push((path.clone(), package.digest));
        }
        writeln!(self.output, "{} {} ({})", package.name, package.version, path.display())
            .map_err(|error| format!("failed to format package graph: {error}"))?;
        for (alias, relative) in edges {
            writeln!(self.output, "  {alias} -> {relative}")
                .map_err(|error| format!("failed to format package graph: {error}"))?;
        }
        Ok(package.version)
    }
}

fn parse_version(value: &str, allow_missing_patch: bool) -> Option<(u64, u64, u64)> {
    fn component(text: &str) -> Option<u64> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit())
            || (text.len() > 1 && text.starts_with('0'))
        {
            return None;
        }
        text.parse().ok()
    }

    let mut parts = value.split('.');
    let major = component(parts.next()?)?;
    let minor = component(parts.next()?)?;
    let patch = match parts.next() {
        Some(text) => component(text)?,
        None if allow_missing_patch => 0,
        None => return None,
    };
    (parts.next().is_none()).then_some((major, minor, patch))
}

/// Resolves a recorded dependency path against the canonical package
/// directory. The base carries a Windows verbatim (`\\?\`) prefix after
/// canonicalization, and the OS refuses to fold `..` inside verbatim paths, so
/// `base.join(recorded).canonicalize()` would fail for `../name` entries; the
/// `..` components are folded lexically instead.
fn resolve_recorded_path(base: &Path, recorded: &str) -> PathBuf {
    let recorded = Path::new(recorded);
    if recorded.is_absolute() {
        return recorded.to_path_buf();
    }
    let mut resolved = base.to_path_buf();
    for component in recorded.components() {
        match component {
            std::path::Component::ParentDir => { resolved.pop(); }
            std::path::Component::CurDir => {}
            _ => resolved.push(component.as_os_str()),
        }
    }
    resolved
}

/// Path from one canonical directory to another, using `/` separators so the
/// manifest reads the same on both platforms.
fn relative_directory(from: &Path, to: &Path) -> Result<String, String> {
    let from_parts: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let shared = from_parts.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    if shared == 0 {
        return Err(format!("cannot express {} relative to {}", to.display(), from.display()));
    }
    let mut parts = vec!["..".to_owned(); from_parts.len() - shared];
    for component in &to_parts[shared..] {
        let text = component.as_os_str().to_str()
            .ok_or_else(|| format!("cannot write a non-UTF-8 dependency path: {}", to.display()))?;
        parts.push(text.to_owned());
    }
    Ok(if parts.is_empty() { ".".to_owned() } else { parts.join("/") })
}

fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Inserts one dependency line at the top of `[dependencies]`, creating the
/// section when the manifest has none. The rest of the file keeps its text.
fn insert_dependency_line(source: &str, line: &str) -> String {
    let mut out = String::new();
    let mut inserted = false;
    for existing in source.lines() {
        out.push_str(existing);
        out.push('\n');
        if !inserted && existing.trim() == "[dependencies]" {
            out.push_str(line);
            out.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        if !out.ends_with('\n') { out.push('\n'); }
        out.push_str("\n[dependencies]\n");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn dependency_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('#') || line.starts_with('[') { return None; }
    let (key, _) = line.split_once('=')?;
    Some(key.trim())
}

fn remove_dependency_line(source: &str, alias: &str) -> (String, bool) {
    let mut out = String::new();
    let mut in_dependencies = false;
    let mut removed = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') { in_dependencies = trimmed == "[dependencies]"; }
        if in_dependencies && dependency_key(line) == Some(alias) {
            removed = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, removed)
}

/// Adds a path dependency named after the target package. The manifest text is
/// edited in place (only the `[dependencies]` section grows) and the lockfile is
/// refreshed so an ordinary build keeps working.
pub(super) fn add_dependency(input: &Path, target: &Path) -> Result<String, String> {
    let manifest = manifest_path(input)
        .ok_or_else(|| format!("{} is not a package directory or {MANIFEST}", input.display()))?
        .canonicalize().map_err(|error| format!("{}: {error}", input.display()))?;
    let root = manifest.parent()
        .ok_or_else(|| format!("{}: manifest path has no parent directory", manifest.display()))?;
    let target_manifest = if target.is_dir() { target.join(MANIFEST) } else { target.to_owned() };
    let target_manifest = target_manifest.canonicalize()
        .map_err(|error| format!("{}: {error}", target_manifest.display()))?;
    if target_manifest == manifest {
        return Err(format!("{}: a package cannot depend on itself", manifest.display()));
    }
    let package = read_manifest(&target_manifest)?;
    let alias = package.name.clone();
    let target_directory = target_manifest.parent()
        .ok_or_else(|| format!("{}: manifest path has no parent directory", target_manifest.display()))?;

    let source = fs::read_to_string(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?;
    let current = read_manifest(&manifest)?;
    if let Some(value) = current.dependencies.get(&alias) {
        let existing = value.as_table().and_then(|table| table.get("path")).and_then(toml::Value::as_str);
        let same = existing.map(|recorded| {
            resolve_recorded_path(root, recorded).canonicalize().ok() == Some(target_directory.to_owned())
        }).unwrap_or(false);
        if same {
            return Ok(format!("'{alias}' is already declared"));
        }
        return Err(format!("{MANIFEST}: dependency '{alias}' is already declared with a different source"));
    }
    let relative = relative_directory(root, target_directory)?;
    let line = format!("{alias} = {{ path = {} }}", toml_string(&relative));
    let updated = insert_dependency_line(&source, &line);
    fs::write(&manifest, updated).map_err(|error| format!("{}: {error}", manifest.display()))?;
    update_lock(&manifest)?;
    Ok(format!("added dependency '{alias}' at {relative}"))
}

/// Removes a dependency by alias. The manifest edit is all-or-nothing; the
/// lockfile refresh is best-effort because the package's own sources may still
/// import the removed alias, and those imports have to be edited before the
/// package compiles and a fresh lock can be built. A failed refresh leaves the
/// old lock in place, so the next build reports it as stale instead of
/// silently trusting an outdated graph.
pub(super) fn remove_dependency(input: &Path, alias: &str) -> Result<String, String> {
    let manifest = manifest_path(input)
        .ok_or_else(|| format!("{} is not a package directory or {MANIFEST}", input.display()))?
        .canonicalize().map_err(|error| format!("{}: {error}", input.display()))?;
    let source = fs::read_to_string(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?;
    let current = read_manifest(&manifest)?;
    if !current.dependencies.contains_key(alias) {
        return Err(format!("{}: no dependency named '{alias}'", manifest.display()));
    }
    let (updated, removed) = remove_dependency_line(&source, alias);
    if !removed {
        return Err(format!("{}: dependency '{alias}' is written in a form this command does not edit; remove it by hand",
            manifest.display()));
    }
    fs::write(&manifest, updated).map_err(|error| format!("{}: {error}", manifest.display()))?;
    match update_lock(&manifest) {
        Ok(()) => Ok(format!("removed dependency '{alias}'")),
        Err(error) => Ok(format!("removed dependency '{alias}'\n\
            ae.lock is now stale ({error});\n\
            run 'ae update' after removing imports of '{alias}'")),
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::parse_manifest_str;
    use std::path::Path;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn usize(&mut self, bound: usize) -> usize {
            if bound == 0 {
                0
            } else {
                (self.next() as usize) % bound
            }
        }
    }

    const SEEDS: &[&str] = &[
        "",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"0.1\"\n",
        "[package]\nname = \"bad name!\"\nversion = \"0.1.0\"\nae = \"0.1\"\n",
        "[package]\nname = \"app\"\nversion = \"01.0.0\"\nae = \"0.1\"\n",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"999.0\"\n",
        "package = 123\ndependencies = [1, 2]\ntarget = \"x86_64\"\n",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"0.1\"\n[dependencies]\nfoo = \"^1.0\"\nbar = { path = \"/abs/path\", version = \"bad\" }\n",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"0.1\"\n[target.linux]\ndependencies = { bad = 42 }\n",
    ];

    const FRAGMENTS: &[&str] = &[
        "[package]", "[dependencies]", "[target]", "name = ", "version = ", "ae = ", "path = ",
        "\"0.1.0\"", "\"../dep\"", "\"\"", "123", "true", "[]", "{}", "=", "\n", "\"", "#", "中", "😀",
    ];

    fn mutate_toml(rng: &mut Rng, base: &str) -> String {
        let mut chars: Vec<char> = base.chars().collect();
        let ops = 1 + rng.usize(5);
        for _ in 0..ops {
            match rng.usize(3) {
                0 if !chars.is_empty() => {
                    let len = rng.usize(chars.len() + 1);
                    chars.truncate(len);
                }
                1 if !chars.is_empty() => {
                    let start = rng.usize(chars.len());
                    let end = start + rng.usize(chars.len() - start + 1);
                    chars.drain(start..end);
                }
                _ => {
                    let frag = FRAGMENTS[rng.usize(FRAGMENTS.len())];
                    let pos = rng.usize(chars.len() + 1);
                    chars.splice(pos..pos, frag.chars());
                }
            }
        }
        chars.into_iter().collect()
    }

    #[test]
    fn edition_field_parses_or_rejects() {
        // M8.1.02: a manifest without the field keeps edition 1; only the two
        // published editions are accepted.
        let path = Path::new("edition/ae.toml");
        let parse = |extra: &str| {
            parse_manifest_str(path, &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nae = \"0.1\"\n{extra}"))
        };
        assert_eq!(parse("").unwrap().edition, 1);
        assert_eq!(parse("edition = \"1\"\n").unwrap().edition, 1);
        assert_eq!(parse("edition = \"2\"\n").unwrap().edition, 2);
        assert!(parse("edition = \"3\"\n").is_err());
        assert!(parse("edition = 2\n").is_err());
    }

    #[test]
    fn deterministic_fuzz_manifest_parser_zero_panics() {
        let mut rng = Rng(0x517C_C1B7_2722_0A95);
        let manifest_path = Path::new("fuzz/ae.toml");

        for &seed in SEEDS {
            let _ = parse_manifest_str(manifest_path, seed);
            for (idx, _) in seed.char_indices() {
                let _ = parse_manifest_str(manifest_path, &seed[..idx]);
            }
        }

        for i in 0..2000 {
            let base = SEEDS[i % SEEDS.len()];
            let mutated = mutate_toml(&mut rng, base);
            let _ = parse_manifest_str(manifest_path, &mutated);
        }
    }
}

#[cfg(test)]
mod integrity_tests {
    use super::{extract_verified_archive, pack_archive, parse_lock_archives, verify_cached_archive};
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ae_pkg_integrity_{label}_{}_{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_archive_verifies_and_extracts() {
        let dir = unique_dir("valid");
        let archive_path = dir.join("math-0.1.0.aepkg");
        let extract_dir = dir.join("extracted");
        let bytes = pack_archive(&[
            ("src/lib.ae", b"pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n"),
            ("ae.toml", b"[package]\nname = \"math\"\nversion = \"0.1.0\"\nae = \"0.1\"\n"),
        ])
        .unwrap();
        let digest = ae_compiler::sha256::hex(&bytes);
        fs::write(&archive_path, &bytes).unwrap();

        extract_verified_archive("math", &archive_path, &digest, &extract_dir).unwrap();
        assert!(extract_dir.join("ae.toml").is_file());
        assert!(extract_dir.join("src/lib.ae").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tampered_one_byte_fails_closed_before_extraction() {
        let dir = unique_dir("tamper");
        let archive_path = dir.join("math-0.1.0.aepkg");
        let extract_dir = dir.join("extracted");
        let mut bytes = pack_archive(&[
            ("ae.toml", b"[package]\nname = \"math\"\nversion = \"0.1.0\"\nae = \"0.1\"\n"),
            ("src/lib.ae", b"pub fn v() -> i64 { return 1; }\n"),
        ])
        .unwrap();
        let expected = ae_compiler::sha256::hex(&bytes);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let observed = ae_compiler::sha256::hex(&bytes);
        fs::write(&archive_path, &bytes).unwrap();

        let err = extract_verified_archive("math", &archive_path, &expected, &extract_dir).unwrap_err();
        assert!(err.contains("math"), "{err}");
        assert!(err.contains("math-0.1.0.aepkg"), "{err}");
        assert!(err.contains(&expected), "{err}");
        assert!(err.contains(&observed), "{err}");
        assert!(!extract_dir.exists(), "extraction directory must not be created on checksum mismatch");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_missing_and_malformed_checksums_fail_closed() {
        let dir = unique_dir("edge");
        let missing_path = dir.join("missing.aepkg");
        let extract_dir = dir.join("extracted");
        let valid_hash = "0".repeat(64);

        let err_missing = verify_cached_archive("math", &missing_path, &valid_hash).unwrap_err();
        assert!(err_missing.contains("math") && err_missing.contains("missing.aepkg"), "{err_missing}");

        let err_malformed = verify_cached_archive("math", &missing_path, "not-a-sha256").unwrap_err();
        assert!(err_malformed.contains("math") && err_malformed.contains("not-a-sha256"), "{err_malformed}");

        let truncated_path = dir.join("truncated.aepkg");
        let mut bytes = pack_archive(&[("ae.toml", b"hello")]).unwrap();
        bytes.truncate(bytes.len() - 2);
        let trunc_hash = ae_compiler::sha256::hex(&bytes);
        fs::write(&truncated_path, &bytes).unwrap();
        let err_trunc = extract_verified_archive("math", &truncated_path, &trunc_hash, &extract_dir).unwrap_err();
        assert!(err_trunc.contains("truncated") && !extract_dir.exists(), "{err_trunc}");

        let lock_path = dir.join("ae.lock");
        fs::write(&lock_path, "ae-lock-v1\narchive math 0.1.0 badhash \"math-0.1.0.aepkg\"\n").unwrap();
        let err_lock = parse_lock_archives(&lock_path).unwrap_err();
        assert!(err_lock.contains("math") && err_lock.contains("badhash"), "{err_lock}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_traversal_and_absolute_paths_are_rejected() {
        assert!(pack_archive(&[("../evil.ae", b"x")]).is_err());
        assert!(pack_archive(&[("/etc/passwd", b"x")]).is_err());
        assert!(pack_archive(&[("C:\\evil.ae", b"x")]).is_err());
        assert!(pack_archive(&[("a\\b.ae", b"x")]).is_err());
    }
}

