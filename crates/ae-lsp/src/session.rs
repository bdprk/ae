//! Incremental LSP document session backed by [`ae_query::QueryDb`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ae_query::{CheckSummary, FileId, QueryDb, QueryError};

use crate::position::{LineIndex, Position, Range};

/// Cooperative cancellation token checked before and during LSP queries.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Create a new uncancelled [`CancelToken`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation to any query observing this token.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Return `true` if cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Errors returned by [`LspSession`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The operation was aborted because its [`CancelToken`] was cancelled.
    Cancelled,
    /// The requested document URI is not currently open in the session.
    UnknownDocument(String),
    /// An underlying [`QueryDb`] error occurred.
    Query(QueryError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("LSP request cancelled"),
            Self::UnknownDocument(uri) => write!(f, "unknown LSP document: {uri}"),
            Self::Query(err) => write!(f, "query error: {err}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<QueryError> for SessionError {
    fn from(err: QueryError) -> Self {
        Self::Query(err)
    }
}

/// Incremental or full-document change event matching LSP `TextDocumentContentChangeEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDocumentContentChangeEvent {
    /// When `Some`, replaces the given UTF-16 range; when `None`, replaces the entire document.
    pub range: Option<Range>,
    pub text: String,
}

impl TextDocumentContentChangeEvent {
    /// Construct a full-document replacement event.
    #[must_use]
    pub fn full(text: impl Into<String>) -> Self {
        Self {
            range: None,
            text: text.into(),
        }
    }

    /// Construct an incremental UTF-16 range replacement event.
    #[must_use]
    pub fn incremental(range: Range, text: impl Into<String>) -> Self {
        Self {
            range: Some(range),
            text: text.into(),
        }
    }
}

/// LSP diagnostic with UTF-16 [`Range`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub range: Range,
    pub severity: u32,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
struct DocumentState {
    file_id: FileId,
    version: i64,
    text: String,
    line_index: LineIndex,
}

/// Stateful LSP session managing open documents and incremental queries over [`QueryDb`].
#[derive(Debug, Clone, Default)]
pub struct LspSession {
    db: QueryDb,
    next_file_id: u32,
    uri_to_file: BTreeMap<String, FileId>,
    documents: BTreeMap<String, DocumentState>,
}

impl LspSession {
    /// Create an empty [`LspSession`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the underlying [`QueryDb`].
    #[must_use]
    pub fn db(&self) -> &QueryDb {
        &self.db
    }

    /// Mutably borrow the underlying [`QueryDb`].
    pub fn db_mut(&mut self) -> &mut QueryDb {
        &mut self.db
    }

    /// Open (or re-open) a document in the session and register its source in [`QueryDb`].
    pub fn did_open(
        &mut self,
        uri: impl Into<String>,
        version: i64,
        text: impl Into<String>,
    ) -> FileId {
        let uri = uri.into();
        let text = text.into();
        let file_id = if let Some(&existing) = self.uri_to_file.get(&uri) {
            existing
        } else {
            let id = FileId(self.next_file_id);
            self.next_file_id = self.next_file_id.saturating_add(1);
            self.uri_to_file.insert(uri.clone(), id);
            id
        };

        let line_index = LineIndex::new(&text);
        self.db.set_source(file_id, uri.clone(), text.clone());
        self.documents.insert(
            uri,
            DocumentState {
                file_id,
                version,
                text,
                line_index,
            },
        );
        file_id
    }

    /// Apply a sequence of full or incremental UTF-16 range changes to an open document.
    pub fn did_change(
        &mut self,
        uri: &str,
        version: i64,
        changes: &[TextDocumentContentChangeEvent],
    ) -> Result<(), SessionError> {
        let Some(doc) = self.documents.get_mut(uri) else {
            return Err(SessionError::UnknownDocument(uri.to_owned()));
        };

        for change in changes {
            match change.range {
                None => {
                    doc.text.clone_from(&change.text);
                }
                Some(range) => {
                    let byte_range = doc.line_index.range_to_offsets(&doc.text, range);
                    doc.text.replace_range(byte_range, &change.text);
                }
            }
            doc.line_index = LineIndex::new(&doc.text);
        }

        doc.version = version;
        let file_id = doc.file_id;
        let updated_text = doc.text.clone();
        self.db.set_source(file_id, uri, updated_text);
        Ok(())
    }

    /// Close an open document. Returns `true` if the document was open.
    pub fn did_close(&mut self, uri: &str) -> bool {
        self.documents.remove(uri).is_some()
    }

    /// Register a directed import edge from `importer_uri` to `imported_uri`.
    pub fn register_import(
        &mut self,
        importer_uri: &str,
        imported_uri: &str,
    ) -> Result<(), SessionError> {
        let importer = self
            .uri_to_file
            .get(importer_uri)
            .copied()
            .ok_or_else(|| SessionError::UnknownDocument(importer_uri.to_owned()))?;
        let imported = self
            .uri_to_file
            .get(imported_uri)
            .copied()
            .ok_or_else(|| SessionError::UnknownDocument(imported_uri.to_owned()))?;
        self.db.register_import(importer, imported);
        Ok(())
    }

    /// Return `true` if `uri` is currently open.
    #[must_use]
    pub fn is_open(&self, uri: &str) -> bool {
        self.documents.contains_key(uri)
    }

    /// Return the current text of an open document.
    #[must_use]
    pub fn document_text(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(|d| d.text.as_str())
    }

    /// Return the current version of an open document.
    #[must_use]
    pub fn document_version(&self, uri: &str) -> Option<i64> {
        self.documents.get(uri).map(|d| d.version)
    }

    /// Return the [`FileId`] assigned to `uri`, if known.
    #[must_use]
    pub fn file_id(&self, uri: &str) -> Option<FileId> {
        self.uri_to_file.get(uri).copied()
    }

    /// Compute LSP diagnostics for `uri`, respecting `cancel` and leveraging [`QueryDb`].
    pub fn diagnostics(
        &mut self,
        uri: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<LspDiagnostic>, SessionError> {
        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let (file_id, text, line_index) = {
            let doc = self
                .documents
                .get(uri)
                .ok_or_else(|| SessionError::UnknownDocument(uri.to_owned()))?;
            (doc.file_id, doc.text.clone(), doc.line_index.clone())
        };

        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let parsed = self.db.parse_file(file_id)?;

        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let _checked = self.db.check_file(file_id)?;

        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let diagnostics = parsed
            .diagnostics
            .into_iter()
            .map(|diag| {
                let range = diag.primary.as_ref().map_or_else(
                    || Range::new(Position::new(0, 0), Position::new(0, 0)),
                    |label| line_index.offsets_to_range(&text, label.span.range.clone()),
                );
                LspDiagnostic {
                    range,
                    severity: 1,
                    code: diag.code,
                    message: diag.message,
                }
            })
            .collect();

        Ok(diagnostics)
    }

    /// Run [`QueryDb::check_file`] for `uri`, respecting `cancel` and early cutoff.
    pub fn check_with_cancel(
        &mut self,
        uri: &str,
        cancel: &CancelToken,
    ) -> Result<CheckSummary, SessionError> {
        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let file_id = self
            .documents
            .get(uri)
            .map(|d| d.file_id)
            .ok_or_else(|| SessionError::UnknownDocument(uri.to_owned()))?;

        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        let summary = self.db.check_file(file_id)?;

        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }

        Ok(summary)
    }
}
