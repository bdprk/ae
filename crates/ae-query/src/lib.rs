//! Incremental pull-based query engine for AE.
//!
//! Provides Salsa-style memoized queries with automatic dependency edge
//! recording, early cutoff via public-interface fingerprinting, and atomic
//! on-disk cache persistence.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use ae_span::Diagnostic;
use ae_syntax::declaration::{Declaration, DeclarationAttribute, DeclarationKind, TypedName, recover_declarations};
use ae_syntax::module::parse_use_headers;
use ae_syntax::{Kind, LexOutcome, lex_recovered};

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;
const CACHE_MAGIC: &[u8; 8] = b"AEQRY001";
const CACHE_VERSION: u32 = 1;

/// Unique identifier for a source file in [`QueryDb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// Monotonically increasing database revision number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Revision(pub u64);

/// Deterministic 64-bit FNV-1a content fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub u64);

impl Fingerprint {
    /// Compute the 64-bit FNV-1a fingerprint of a byte slice.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = FingerprintHasher::new();
        hasher.write_raw(bytes);
        hasher.finish()
    }
}

/// Incremental builder for deterministic 64-bit FNV-1a [`Fingerprint`] values.
#[derive(Debug, Clone)]
pub struct FingerprintHasher {
    state: u64,
}

impl Default for FingerprintHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl FingerprintHasher {
    /// Create a hasher initialized to the 64-bit FNV-1a offset basis.
    #[must_use]
    pub fn new() -> Self {
        Self { state: FNV1A_OFFSET_BASIS }
    }

    /// Feed raw bytes into the FNV-1a state without a length prefix.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state ^= u64::from(byte);
            self.state = self.state.wrapping_mul(FNV1A_PRIME);
        }
    }

    /// Feed a length-prefixed byte slice to prevent boundary collisions.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_usize(bytes.len());
        self.write_raw(bytes);
    }

    /// Feed a length-prefixed string.
    pub fn write_str(&mut self, text: &str) {
        self.write_bytes(text.as_bytes());
    }

    /// Feed a `u64` in little-endian byte order.
    pub fn write_u64(&mut self, value: u64) {
        self.write_raw(&value.to_le_bytes());
    }

    /// Feed a `u32` in little-endian byte order.
    pub fn write_u32(&mut self, value: u32) {
        self.write_raw(&value.to_le_bytes());
    }

    /// Feed a `usize` normalized as `u64` in little-endian byte order.
    pub fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    /// Feed a boolean byte.
    pub fn write_bool(&mut self, value: bool) {
        self.write_raw(&[u8::from(value)]);
    }

    /// Finalize and return the computed [`Fingerprint`].
    #[must_use]
    pub fn finish(&self) -> Fingerprint {
        Fingerprint(self.state)
    }
}

/// Discriminant for queries tracked by [`QueryDb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QueryKind {
    SourceInput,
    ParseFile,
    PublicInterfaceFingerprint,
    CheckFile,
}

impl QueryKind {
    fn to_tag(self) -> u8 {
        match self {
            Self::SourceInput => 1,
            Self::ParseFile => 2,
            Self::PublicInterfaceFingerprint => 3,
            Self::CheckFile => 4,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::SourceInput),
            2 => Some(Self::ParseFile),
            3 => Some(Self::PublicInterfaceFingerprint),
            4 => Some(Self::CheckFile),
            _ => None,
        }
    }
}

/// Unique key identifying a query invocation for a specific [`FileId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryKey {
    pub kind: QueryKind,
    pub file: FileId,
}

/// Errors returned by [`QueryDb`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The requested [`FileId`] has not been registered via `set_source`.
    UnknownFile(FileId),
    /// An I/O operation failed while saving or loading the on-disk cache.
    Io(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFile(file) => write!(f, "unknown file id {}", file.0),
            Self::Io(message) => write!(f, "query cache I/O error: {message}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Execution counters across query kinds for observability and testing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub parse_executions: u64,
    pub interface_executions: u64,
    pub check_executions: u64,
}

/// Result of parsing a single source file via `ae_syntax`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseSummary {
    pub file: FileId,
    pub path: String,
    pub imports: Vec<String>,
    pub declarations: Vec<Declaration>,
    pub diagnostics: Vec<Diagnostic>,
    pub fingerprint: Fingerprint,
}

/// Result of checking a file against its own AST and imported public interfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub file: FileId,
    pub declaration_count: usize,
    pub diagnostic_count: usize,
    pub imported_interfaces: Vec<(FileId, Fingerprint)>,
    pub fingerprint: Fingerprint,
}

/// Outcome of loading a persisted cache file from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLoadStatus {
    Loaded,
    NotFound,
    CorruptedRecovered,
}

#[derive(Debug, Clone)]
struct SourceEntry {
    path: String,
    text: String,
    fingerprint: Fingerprint,
    changed_at: Revision,
}

#[derive(Debug, Clone)]
struct MemoEntry<T> {
    value: T,
    value_fingerprint: Fingerprint,
    verified_at: Revision,
    changed_at: Revision,
    dependencies: Vec<(QueryKey, Fingerprint)>,
}

/// Pull-based incremental query database with automatic dependency recording
/// and early cutoff.
#[derive(Debug, Clone)]
pub struct QueryDb {
    revision: Revision,
    sources: BTreeMap<FileId, SourceEntry>,
    path_to_file: BTreeMap<String, FileId>,
    explicit_imports: BTreeMap<FileId, Vec<FileId>>,
    imports_changed_at: BTreeMap<FileId, Revision>,
    parse_cache: BTreeMap<FileId, MemoEntry<ParseSummary>>,
    interface_cache: BTreeMap<FileId, MemoEntry<Fingerprint>>,
    check_cache: BTreeMap<FileId, MemoEntry<CheckSummary>>,
    active_deps: Vec<Vec<(QueryKey, Fingerprint)>>,
    recompute_counts: BTreeMap<QueryKey, u64>,
    stats: QueryStats,
}

impl Default for QueryDb {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryDb {
    /// Create an empty [`QueryDb`] at revision 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            revision: Revision(0),
            sources: BTreeMap::new(),
            path_to_file: BTreeMap::new(),
            explicit_imports: BTreeMap::new(),
            imports_changed_at: BTreeMap::new(),
            parse_cache: BTreeMap::new(),
            interface_cache: BTreeMap::new(),
            check_cache: BTreeMap::new(),
            active_deps: Vec::new(),
            recompute_counts: BTreeMap::new(),
            stats: QueryStats::default(),
        }
    }

    /// Return the current database revision.
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// Return aggregate query execution statistics.
    #[must_use]
    pub fn stats(&self) -> QueryStats {
        self.stats
    }

    /// Return how many times a specific query key has been executed (recomputed).
    #[must_use]
    pub fn recompute_count(&self, key: QueryKey) -> u64 {
        self.recompute_counts.get(&key).copied().unwrap_or(0)
    }

    /// Set or update the source text for `file`, advancing [`Revision`] when
    /// the path or text changes.
    pub fn set_source(
        &mut self,
        file: FileId,
        path_or_name: impl Into<String>,
        text: impl Into<String>,
    ) {
        let path = path_or_name.into();
        let text = text.into();
        let fingerprint = fingerprint_source(&path, &text);

        if let Some(existing) = self.sources.get(&file) {
            if existing.fingerprint == fingerprint && existing.path == path && existing.text == text {
                return;
            }
            if existing.path != path {
                self.path_to_file.remove(&existing.path);
            }
        }

        self.revision.0 += 1;
        let rev = self.revision;
        self.path_to_file.insert(path.clone(), file);
        self.sources.insert(
            file,
            SourceEntry { path, text, fingerprint, changed_at: rev },
        );
    }

    /// Register a directed import edge from `importer` to `imported`.
    pub fn register_import(&mut self, importer: FileId, imported: FileId) {
        let entry = self.explicit_imports.entry(importer).or_default();
        if !entry.contains(&imported) {
            entry.push(imported);
            entry.sort_unstable();
            self.revision.0 += 1;
            self.imports_changed_at.insert(importer, self.revision);
        }
    }

    /// Return the recorded dependency edges for `file`.
    #[must_use]
    pub fn dependencies_of(&self, file: FileId) -> Vec<QueryKey> {
        if let Some(entry) = self.check_cache.get(&file) {
            return entry.dependencies.iter().map(|(key, _)| *key).collect();
        }
        if let Some(entry) = self.interface_cache.get(&file) {
            return entry.dependencies.iter().map(|(key, _)| *key).collect();
        }
        if let Some(entry) = self.parse_cache.get(&file) {
            return entry.dependencies.iter().map(|(key, _)| *key).collect();
        }
        Vec::new()
    }

    /// Parse `file` via `ae_syntax`, returning its AST declaration summary and
    /// diagnostics. Memoized with automatic dependency tracking.
    pub fn parse_file(&mut self, file: FileId) -> Result<ParseSummary, QueryError> {
        let (summary, fp) = self.ensure_parse_file(file)?;
        self.record_dep(QueryKey { kind: QueryKind::ParseFile, file }, fp);
        Ok(summary)
    }

    /// Compute the deterministic fingerprint of `file`'s public interface,
    /// deliberately ignoring private declarations, function bodies, comments,
    /// and whitespace.
    pub fn public_interface_fingerprint(&mut self, file: FileId) -> Result<Fingerprint, QueryError> {
        let fp = self.ensure_public_interface(file)?;
        self.record_dep(
            QueryKey { kind: QueryKind::PublicInterfaceFingerprint, file },
            fp,
        );
        Ok(fp)
    }

    /// Check `file` using its own `parse_file` result and the
    /// `public_interface_fingerprint` of every imported dependency.
    pub fn check_file(&mut self, file: FileId) -> Result<CheckSummary, QueryError> {
        let (summary, fp) = self.ensure_check_file(file)?;
        self.record_dep(QueryKey { kind: QueryKind::CheckFile, file }, fp);
        Ok(summary)
    }

    /// Atomically persist the database state to `path` using a temporary file
    /// and rename, protected by a magic header and trailing FNV-1a checksum.
    pub fn save_cache(&self, path: &Path) -> Result<(), QueryError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| QueryError::Io(err.to_string()))?;
        }

        let payload = self.encode_cache();
        let tmp_name = format!(
            ".{}.tmp.{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("ae_query_cache"),
            std::process::id()
        );
        let tmp_path = path.with_file_name(tmp_name);

        let write_result = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(QueryError::Io(err.to_string()));
        }

        if let Err(err) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(QueryError::Io(err.to_string()));
        }

        Ok(())
    }

    /// Load a persisted [`QueryDb`] from `path`, recovering cleanly into
    /// [`CacheLoadStatus::CorruptedRecovered`] on truncated or bit-flipped data.
    pub fn load_cache(path: &Path) -> Result<(Self, CacheLoadStatus), QueryError> {
        let mut bytes = Vec::new();
        match File::open(path) {
            Ok(mut file) => {
                file.read_to_end(&mut bytes)
                    .map_err(|err| QueryError::Io(err.to_string()))?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::new(), CacheLoadStatus::NotFound));
            }
            Err(err) => return Err(QueryError::Io(err.to_string())),
        }

        match Self::decode_cache(&bytes) {
            Some(db) => Ok((db, CacheLoadStatus::Loaded)),
            None => Ok((Self::new(), CacheLoadStatus::CorruptedRecovered)),
        }
    }

    /// Replace `self` with the cache loaded from `path`.
    pub fn reload_cache(&mut self, path: &Path) -> Result<CacheLoadStatus, QueryError> {
        let (db, status) = Self::load_cache(path)?;
        *self = db;
        Ok(status)
    }

    fn record_dep(&mut self, key: QueryKey, fp: Fingerprint) {
        if let Some(frame) = self.active_deps.last_mut()
            && !frame.iter().any(|(existing, _)| *existing == key)
        {
            frame.push((key, fp));
        }
    }

    fn bump_recompute(&mut self, key: QueryKey) {
        *self.recompute_counts.entry(key).or_insert(0) += 1;
        match key.kind {
            QueryKind::SourceInput => {}
            QueryKind::ParseFile => self.stats.parse_executions += 1,
            QueryKind::PublicInterfaceFingerprint => self.stats.interface_executions += 1,
            QueryKind::CheckFile => self.stats.check_executions += 1,
        }
    }

    fn ensure_query_fp(&mut self, key: QueryKey) -> Result<Fingerprint, QueryError> {
        match key.kind {
            QueryKind::SourceInput => {
                let source = self.sources.get(&key.file).ok_or(QueryError::UnknownFile(key.file))?;
                Ok(source.fingerprint)
            }
            QueryKind::ParseFile => self.ensure_parse_file(key.file).map(|(_, fp)| fp),
            QueryKind::PublicInterfaceFingerprint => self.ensure_public_interface(key.file),
            QueryKind::CheckFile => self.ensure_check_file(key.file).map(|(_, fp)| fp),
        }
    }

    fn dependencies_unchanged(&mut self, deps: &[(QueryKey, Fingerprint)]) -> Result<bool, QueryError> {
        for &(dep_key, recorded_fp) in deps {
            let current_fp = self.ensure_query_fp(dep_key)?;
            if current_fp != recorded_fp {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn ensure_parse_file(&mut self, file: FileId) -> Result<(ParseSummary, Fingerprint), QueryError> {
        if let Some(entry) = self.parse_cache.get(&file)
            && entry.verified_at == self.revision
        {
            return Ok((entry.value.clone(), entry.value_fingerprint));
        }

        if let Some(existing) = self.parse_cache.get(&file) {
            let deps = existing.dependencies.clone();
            if self.dependencies_unchanged(&deps)?
                && let Some(entry) = self.parse_cache.get_mut(&file)
            {
                entry.verified_at = self.revision;
                return Ok((entry.value.clone(), entry.value_fingerprint));
            }
        }

        let source = self
            .sources
            .get(&file)
            .cloned()
            .ok_or(QueryError::UnknownFile(file))?;

        self.active_deps.push(Vec::new());
        let source_key = QueryKey { kind: QueryKind::SourceInput, file };
        self.record_dep(source_key, source.fingerprint);

        self.bump_recompute(QueryKey { kind: QueryKind::ParseFile, file });
        let imports = extract_imports(&source.path, &source.text);
        let recovered = recover_declarations(&source.path, &source.text);
        let value_fingerprint = fingerprint_parse_summary(
            source.fingerprint,
            &source.path,
            &imports,
            &recovered.declarations,
            &recovered.diagnostics,
        );
        let summary = ParseSummary {
            file,
            path: source.path,
            imports,
            declarations: recovered.declarations,
            diagnostics: recovered.diagnostics,
            fingerprint: value_fingerprint,
        };
        let deps = self.active_deps.pop().unwrap_or_default();

        let prev_fp = self.parse_cache.get(&file).map(|e| e.value_fingerprint);
        let prev_changed = self.parse_cache.get(&file).map(|e| e.changed_at);
        let changed_at = if prev_fp == Some(value_fingerprint) {
            prev_changed.unwrap_or(self.revision)
        } else {
            self.revision
        };

        self.parse_cache.insert(
            file,
            MemoEntry {
                value: summary.clone(),
                value_fingerprint,
                verified_at: self.revision,
                changed_at,
                dependencies: deps,
            },
        );

        Ok((summary, value_fingerprint))
    }

    fn ensure_public_interface(&mut self, file: FileId) -> Result<Fingerprint, QueryError> {
        if let Some(entry) = self.interface_cache.get(&file)
            && entry.verified_at == self.revision
        {
            return Ok(entry.value_fingerprint);
        }

        if let Some(existing) = self.interface_cache.get(&file) {
            let deps = existing.dependencies.clone();
            if self.dependencies_unchanged(&deps)?
                && let Some(entry) = self.interface_cache.get_mut(&file)
            {
                entry.verified_at = self.revision;
                return Ok(entry.value_fingerprint);
            }
        }

        self.active_deps.push(Vec::new());
        let (parsed, parse_fp) = self.ensure_parse_file(file)?;
        self.record_dep(QueryKey { kind: QueryKind::ParseFile, file }, parse_fp);

        self.bump_recompute(QueryKey {
            kind: QueryKind::PublicInterfaceFingerprint,
            file,
        });
        let source_text = self
            .sources
            .get(&file)
            .map(|s| s.text.as_str())
            .ok_or(QueryError::UnknownFile(file))?;
        let iface_fp = fingerprint_public_declarations(source_text, &parsed.declarations);
        let deps = self.active_deps.pop().unwrap_or_default();

        let prev_fp = self.interface_cache.get(&file).map(|e| e.value_fingerprint);
        let prev_changed = self.interface_cache.get(&file).map(|e| e.changed_at);
        let changed_at = if prev_fp == Some(iface_fp) {
            prev_changed.unwrap_or(self.revision)
        } else {
            self.revision
        };

        self.interface_cache.insert(
            file,
            MemoEntry {
                value: iface_fp,
                value_fingerprint: iface_fp,
                verified_at: self.revision,
                changed_at,
                dependencies: deps,
            },
        );

        Ok(iface_fp)
    }

    fn ensure_check_file(&mut self, file: FileId) -> Result<(CheckSummary, Fingerprint), QueryError> {
        let imports_changed_at = self.imports_changed_at.get(&file).copied().unwrap_or(Revision(0));

        if let Some(entry) = self.check_cache.get(&file)
            && entry.verified_at == self.revision
        {
            return Ok((entry.value.clone(), entry.value_fingerprint));
        }

        if let Some(existing) = self.check_cache.get(&file)
            && existing.verified_at >= imports_changed_at
        {
            let deps = existing.dependencies.clone();
            if self.dependencies_unchanged(&deps)?
                && let Some(entry) = self.check_cache.get_mut(&file)
            {
                entry.verified_at = self.revision;
                return Ok((entry.value.clone(), entry.value_fingerprint));
            }
        }

        if !self.sources.contains_key(&file) {
            return Err(QueryError::UnknownFile(file));
        }

        self.active_deps.push(Vec::new());
        let (parsed, parse_fp) = self.ensure_parse_file(file)?;
        self.record_dep(QueryKey { kind: QueryKind::ParseFile, file }, parse_fp);

        let mut imported_ids = self.explicit_imports.get(&file).cloned().unwrap_or_default();
        for import_path in &parsed.imports {
            if let Some(&dep_id) = self.path_to_file.get(import_path)
                && dep_id != file
                && !imported_ids.contains(&dep_id)
            {
                imported_ids.push(dep_id);
            }
        }
        imported_ids.sort_unstable();

        let mut imported_interfaces = Vec::with_capacity(imported_ids.len());
        for dep_id in imported_ids {
            let dep_fp = self.ensure_public_interface(dep_id)?;
            self.record_dep(
                QueryKey { kind: QueryKind::PublicInterfaceFingerprint, file: dep_id },
                dep_fp,
            );
            imported_interfaces.push((dep_id, dep_fp));
        }

        self.bump_recompute(QueryKey { kind: QueryKind::CheckFile, file });
        let value_fingerprint = fingerprint_check_summary(
            parse_fp,
            parsed.declarations.len(),
            parsed.diagnostics.len(),
            &imported_interfaces,
        );
        let summary = CheckSummary {
            file,
            declaration_count: parsed.declarations.len(),
            diagnostic_count: parsed.diagnostics.len(),
            imported_interfaces,
            fingerprint: value_fingerprint,
        };
        let deps = self.active_deps.pop().unwrap_or_default();

        let prev_fp = self.check_cache.get(&file).map(|e| e.value_fingerprint);
        let prev_changed = self.check_cache.get(&file).map(|e| e.changed_at);
        let changed_at = if prev_fp == Some(value_fingerprint) {
            prev_changed.unwrap_or(self.revision)
        } else {
            self.revision
        };

        self.check_cache.insert(
            file,
            MemoEntry {
                value: summary.clone(),
                value_fingerprint,
                verified_at: self.revision,
                changed_at,
                dependencies: deps,
            },
        );

        Ok((summary, value_fingerprint))
    }

    fn encode_cache(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(CACHE_MAGIC);
        write_u32(&mut out, CACHE_VERSION);
        write_u64(&mut out, self.revision.0);

        write_u32(&mut out, self.sources.len() as u32);
        for (&file_id, source) in &self.sources {
            write_u32(&mut out, file_id.0);
            write_str(&mut out, &source.path);
            write_str(&mut out, &source.text);
            write_u64(&mut out, source.fingerprint.0);
            write_u64(&mut out, source.changed_at.0);
            let imports = self.explicit_imports.get(&file_id);
            write_u32(&mut out, imports.map_or(0, Vec::len) as u32);
            if let Some(list) = imports {
                for &dep in list {
                    write_u32(&mut out, dep.0);
                }
            }
        }

        write_u32(&mut out, self.interface_cache.len() as u32);
        for (&file_id, entry) in &self.interface_cache {
            write_u32(&mut out, file_id.0);
            write_u64(&mut out, entry.value_fingerprint.0);
            write_u64(&mut out, entry.verified_at.0);
            write_u64(&mut out, entry.changed_at.0);
            write_deps(&mut out, &entry.dependencies);
        }

        write_u32(&mut out, self.check_cache.len() as u32);
        for (&file_id, entry) in &self.check_cache {
            write_u32(&mut out, file_id.0);
            write_u32(&mut out, entry.value.declaration_count as u32);
            write_u32(&mut out, entry.value.diagnostic_count as u32);
            write_u32(&mut out, entry.value.imported_interfaces.len() as u32);
            for &(dep_id, dep_fp) in &entry.value.imported_interfaces {
                write_u32(&mut out, dep_id.0);
                write_u64(&mut out, dep_fp.0);
            }
            write_u64(&mut out, entry.value_fingerprint.0);
            write_u64(&mut out, entry.verified_at.0);
            write_u64(&mut out, entry.changed_at.0);
            write_deps(&mut out, &entry.dependencies);
        }

        let checksum = Fingerprint::from_bytes(&out);
        write_u64(&mut out, checksum.0);
        out
    }

    fn decode_cache(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CACHE_MAGIC.len() + 4 + 8 {
            return None;
        }
        let payload_len = bytes.len().checked_sub(8)?;
        let (payload, checksum_bytes) = bytes.split_at(payload_len);
        let expected_checksum = u64::from_le_bytes(checksum_bytes.try_into().ok()?);
        if Fingerprint::from_bytes(payload).0 != expected_checksum {
            return None;
        }

        let mut reader = SliceReader::new(payload);
        if reader.read_bytes(CACHE_MAGIC.len())? != CACHE_MAGIC {
            return None;
        }
        if reader.read_u32()? != CACHE_VERSION {
            return None;
        }

        let mut db = Self::new();
        db.revision = Revision(reader.read_u64()?);

        let source_count = reader.read_u32()? as usize;
        for _ in 0..source_count {
            let file_id = FileId(reader.read_u32()?);
            let path = reader.read_str()?;
            let text = reader.read_str()?;
            let fingerprint = Fingerprint(reader.read_u64()?);
            let changed_at = Revision(reader.read_u64()?);
            let import_count = reader.read_u32()? as usize;
            let mut imports = Vec::with_capacity(import_count);
            for _ in 0..import_count {
                imports.push(FileId(reader.read_u32()?));
            }
            db.path_to_file.insert(path.clone(), file_id);
            db.sources.insert(
                file_id,
                SourceEntry { path, text, fingerprint, changed_at },
            );
            if !imports.is_empty() {
                db.explicit_imports.insert(file_id, imports);
            }
        }

        let iface_count = reader.read_u32()? as usize;
        for _ in 0..iface_count {
            let file_id = FileId(reader.read_u32()?);
            let fp = Fingerprint(reader.read_u64()?);
            let verified_at = Revision(reader.read_u64()?);
            let changed_at = Revision(reader.read_u64()?);
            let dependencies = reader.read_deps()?;
            db.interface_cache.insert(
                file_id,
                MemoEntry {
                    value: fp,
                    value_fingerprint: fp,
                    verified_at,
                    changed_at,
                    dependencies,
                },
            );
        }

        let check_count = reader.read_u32()? as usize;
        for _ in 0..check_count {
            let file_id = FileId(reader.read_u32()?);
            let declaration_count = reader.read_u32()? as usize;
            let diagnostic_count = reader.read_u32()? as usize;
            let iface_len = reader.read_u32()? as usize;
            let mut imported_interfaces = Vec::with_capacity(iface_len);
            for _ in 0..iface_len {
                let dep_id = FileId(reader.read_u32()?);
                let dep_fp = Fingerprint(reader.read_u64()?);
                imported_interfaces.push((dep_id, dep_fp));
            }
            let value_fingerprint = Fingerprint(reader.read_u64()?);
            let verified_at = Revision(reader.read_u64()?);
            let changed_at = Revision(reader.read_u64()?);
            let dependencies = reader.read_deps()?;
            let summary = CheckSummary {
                file: file_id,
                declaration_count,
                diagnostic_count,
                imported_interfaces,
                fingerprint: value_fingerprint,
            };
            db.check_cache.insert(
                file_id,
                MemoEntry {
                    value: summary,
                    value_fingerprint,
                    verified_at,
                    changed_at,
                    dependencies,
                },
            );
        }

        if !reader.is_empty() {
            return None;
        }

        Some(db)
    }
}

fn extract_imports(path: &str, source: &str) -> Vec<String> {
    let tokens = match lex_recovered(source) {
        LexOutcome::Clean(tokens) | LexOutcome::Recovered(tokens, _) => tokens,
        LexOutcome::Fatal(_) => return Vec::new(),
    };
    match parse_use_headers(path, source, &tokens) {
        Ok(headers) => headers.paths,
        Err(_) => Vec::new(),
    }
}

fn fingerprint_source(path: &str, text: &str) -> Fingerprint {
    let mut hasher = FingerprintHasher::new();
    hasher.write_str("source");
    hasher.write_str(path);
    hasher.write_str(text);
    hasher.finish()
}

fn fingerprint_parse_summary(
    source_fp: Fingerprint,
    path: &str,
    imports: &[String],
    declarations: &[Declaration],
    diagnostics: &[Diagnostic],
) -> Fingerprint {
    let mut hasher = FingerprintHasher::new();
    hasher.write_str("parse");
    hasher.write_u64(source_fp.0);
    hasher.write_str(path);
    hasher.write_usize(imports.len());
    for import in imports {
        hasher.write_str(import);
    }
    hasher.write_usize(declarations.len());
    for decl in declarations {
        hasher.write_bool(decl.public);
        hasher.write_str(&decl.name);
        hasher.write_usize(decl.range.start);
        hasher.write_usize(decl.range.end);
        if let Some(body) = &decl.body {
            hasher.write_bool(true);
            hasher.write_usize(body.start);
            hasher.write_usize(body.end);
        } else {
            hasher.write_bool(false);
        }
    }
    hasher.write_usize(diagnostics.len());
    for diag in diagnostics {
        hasher.write_str(&diag.message);
    }
    hasher.finish()
}

fn fingerprint_public_declarations(source: &str, declarations: &[Declaration]) -> Fingerprint {
    let mut hasher = FingerprintHasher::new();
    hasher.write_str("public-interface");

    let public_decls: Vec<&Declaration> = declarations.iter().filter(|d| d.public).collect();
    hasher.write_usize(public_decls.len());

    for decl in public_decls {
        hasher.write_usize(decl.attributes.len());
        for attr in &decl.attributes {
            let tag: u8 = match attr {
                DeclarationAttribute::ReprC => 1,
                DeclarationAttribute::NoMangle => 2,
                DeclarationAttribute::DeriveCopy => 3,
            };
            hasher.write_raw(&[tag]);
        }
        hasher.write_str(&decl.name);
        hasher.write_usize(decl.type_parameters.len());
        for tp in &decl.type_parameters {
            hasher.write_str(&tp.name);
            hasher.write_usize(tp.bounds.len());
            for bound in &tp.bounds {
                hasher.write_str(bound);
            }
        }
        match &decl.kind {
            DeclarationKind::Function { parameters, returns, unsafe_fn, abi } => {
                hasher.write_raw(&[1]);
                hasher.write_bool(*unsafe_fn);
                match abi {
                    Some(abi_str) => {
                        hasher.write_bool(true);
                        hasher.write_str(abi_str);
                    }
                    None => hasher.write_bool(false),
                }
                hash_typed_names(&mut hasher, source, parameters);
                match returns {
                    Some(range) => {
                        hasher.write_bool(true);
                        hash_type_range(&mut hasher, source, range.clone());
                    }
                    None => hasher.write_bool(false),
                }
            }
            DeclarationKind::Struct { fields } => {
                hasher.write_raw(&[2]);
                hash_typed_names(&mut hasher, source, fields);
            }
            DeclarationKind::Enum { variants } => {
                hasher.write_raw(&[3]);
                hasher.write_usize(variants.len());
                for variant in variants {
                    hasher.write_str(&variant.name);
                    hasher.write_usize(variant.payload.len());
                    for payload_range in &variant.payload {
                        hash_type_range(&mut hasher, source, payload_range.clone());
                    }
                }
            }
            DeclarationKind::Trait { methods } => {
                hasher.write_raw(&[4]);
                hasher.write_usize(methods.len());
                for method in methods {
                    hasher.write_str(&method.name);
                    hash_typed_names(&mut hasher, source, &method.parameters);
                    hash_type_range(&mut hasher, source, method.returns.clone());
                }
            }
            DeclarationKind::Impl { target } => {
                hasher.write_raw(&[5]);
                match target {
                    Some(range) => {
                        hasher.write_bool(true);
                        hash_type_range(&mut hasher, source, range.clone());
                    }
                    None => hasher.write_bool(false),
                }
            }
        }
    }

    hasher.finish()
}

fn hash_typed_names(hasher: &mut FingerprintHasher, source: &str, items: &[TypedName]) {
    hasher.write_usize(items.len());
    for item in items {
        hasher.write_str(&item.name);
        hasher.write_bool(item.mutable);
        hash_type_range(hasher, source, item.ty.clone());
    }
}

fn hash_type_range(hasher: &mut FingerprintHasher, source: &str, range: std::ops::Range<usize>) {
    let slice = source.get(range).unwrap_or("");
    match lex_recovered(slice) {
        LexOutcome::Clean(tokens) | LexOutcome::Recovered(tokens, _) => {
            hasher.write_usize(tokens.len());
            for token in tokens {
                hash_token_kind(hasher, &token.kind);
            }
        }
        LexOutcome::Fatal(_) => {
            hasher.write_str(slice.trim());
        }
    }
}

fn hash_token_kind(hasher: &mut FingerprintHasher, kind: &Kind) {
    match kind {
        Kind::Ident(s) => {
            hasher.write_raw(&[1]);
            hasher.write_str(s);
        }
        Kind::Number(n) => {
            hasher.write_raw(&[2]);
            hasher.write_u64(*n as u64);
        }
        Kind::Float(f) => {
            hasher.write_raw(&[3]);
            hasher.write_u64(f.to_bits());
        }
        Kind::Text(s) => {
            hasher.write_raw(&[4]);
            hasher.write_str(s);
        }
        Kind::Char(c) => {
            hasher.write_raw(&[5]);
            hasher.write_u32(*c as u32);
        }
        other => {
            hasher.write_raw(&[6]);
            hasher.write_str(&format!("{other:?}"));
        }
    }
}

fn fingerprint_check_summary(
    parse_fp: Fingerprint,
    declaration_count: usize,
    diagnostic_count: usize,
    imported_interfaces: &[(FileId, Fingerprint)],
) -> Fingerprint {
    let mut hasher = FingerprintHasher::new();
    hasher.write_str("check");
    hasher.write_u64(parse_fp.0);
    hasher.write_usize(declaration_count);
    hasher.write_usize(diagnostic_count);
    hasher.write_usize(imported_interfaces.len());
    for &(file_id, fp) in imported_interfaces {
        hasher.write_u32(file_id.0);
        hasher.write_u64(fp.0);
    }
    hasher.finish()
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_str(out: &mut Vec<u8>, text: &str) {
    write_u32(out, text.len() as u32);
    out.extend_from_slice(text.as_bytes());
}

fn write_deps(out: &mut Vec<u8>, deps: &[(QueryKey, Fingerprint)]) {
    write_u32(out, deps.len() as u32);
    for &(key, fp) in deps {
        out.push(key.kind.to_tag());
        write_u32(out, key.file.0);
        write_u64(out, fp.0);
    }
}

struct SliceReader<'a> {
    data: &'a [u8],
}

impl<'a> SliceReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.data.len() < len {
            return None;
        }
        let (head, tail) = self.data.split_at(len);
        self.data = tail;
        Some(head)
    }

    fn read_u8(&mut self) -> Option<u8> {
        let bytes = self.read_bytes(1)?;
        Some(bytes[0])
    }

    fn read_u32(&mut self) -> Option<u32> {
        let bytes = self.read_bytes(4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let bytes = self.read_bytes(8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_str(&mut self) -> Option<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_bytes(len)?;
        std::str::from_utf8(bytes).ok().map(str::to_owned)
    }

    fn read_deps(&mut self) -> Option<Vec<(QueryKey, Fingerprint)>> {
        let len = self.read_u32()? as usize;
        let mut deps = Vec::with_capacity(len.min(1024));
        for _ in 0..len {
            let kind = QueryKind::from_tag(self.read_u8()?)?;
            let file = FileId(self.read_u32()?);
            let fp = Fingerprint(self.read_u64()?);
            deps.push((QueryKey { kind, file }, fp));
        }
        Some(deps)
    }
}
