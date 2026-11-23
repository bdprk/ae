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
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nae = \"{}\"\n",
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
    let mut graph = PathGraph::default();
    let lock_path = manifest.with_file_name("ae.lock");
    let has_lock = lock_path.try_exists().map_err(|error| format!("{}: {error}", lock_path.display()))?;
    let lock = if package.dependencies.is_empty() && !has_lock {
        None
    } else {
        graph.visit(&manifest)?;
        Some(PackageLock { path: lock_path, manifests: graph.manifests })
    };
    Ok(PackageEntry { path: manifest.with_file_name("src").join("main.ae"), imports: graph.imports, lock })
}

pub(super) struct PackageLock {
    path: PathBuf,
    manifests: Vec<(PathBuf, String)>,
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
        for (path, digest) in &self.manifests {
            writeln!(text, "manifest {digest} {:?}", local(path)?)
                .map_err(|error| format!("failed to format lockfile: {error}"))?;
        }
        let mut sources = Vec::new();
        for unit in units {
            let path = unit.path.canonicalize().map_err(|error| format!("{}: {error}", unit.path.display()))?;
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
            PackageLock { path: manifest.with_file_name("ae.lock"), manifests: vec![(canonical, package.digest)] }
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
    dependencies: toml::Table,
    digest: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    Ok(Manifest { name, version, dependencies, digest: ae_compiler::sha256::hex(source.as_bytes()) })
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

pub(super) fn describe_graph(root: &Path) -> Result<String, String> {
    let mut graph = PathGraph::default();
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
}

impl PathGraph {
    fn visit(&mut self, manifest: &Path) -> Result<Version, String> {
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
        self.visiting.push(path.clone());
        let mut edges = Vec::new();
        let mut imports = std::collections::BTreeMap::new();
        for (alias, dependency) in package.dependencies {
            let table = dependency.as_table().ok_or_else(|| {
                format!("{}: dependency resolution for '{alias}' requires a nonempty path table", path.display())
            })?;
            let mut relative = None;
            let mut requirement = None;
            for (key, value) in table {
                match key.as_str() {
                    "path" => relative = value.as_str(),
                    "version" => {
                        let text = value.as_str().ok_or_else(|| format!("{}: dependency '{alias}' version must be a string", path.display()))?;
                        requirement = Some(Requirement::parse(text).ok_or_else(|| format!("{}: dependency '{alias}' version '{text}' must be a major.minor version such as 0.3", path.display()))?);
                    }
                    other => return Err(format!("{}: dependency '{alias}' has unsupported key '{other}'", path.display())),
                }
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
            let version = self.visit(&canonical)?;
            if let Some(requirement) = requirement {
                if !requirement.contains(version) {
                    return Err(format!("{}: dependency '{alias}' requires version {requirement} but {} is {version}",
                        path.display(), canonical.display()));
                }
            }
            imports.insert(alias.clone(), canonical.with_file_name("src").join("lib.ae"));
            edges.push((alias, relative.to_owned()));
        }
        self.visiting.pop();
        self.imports.insert(path.with_file_name("src"), imports);
        self.visited.insert(path.clone(), package.version);
        self.manifests.push((path.clone(), package.digest));
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

