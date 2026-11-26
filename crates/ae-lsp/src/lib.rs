//! Modular LSP foundation for AE backed by `ae-query`.
//!
//! Provides an RFC 8259 JSON parser/serializer with UTF-16 surrogate pair
//! support, exact UTF-8 byte offset <-> LSP UTF-16 `(line, character)`
//! conversion, and an incremental [`LspSession`] over [`ae_query::QueryDb`].

#![forbid(unsafe_code)]

pub mod json;
pub mod position;
pub mod session;

pub use json::{JsonError, JsonErrorKind, JsonValue};
pub use position::{LineIndex, Position, Range};
pub use session::{
    CancelToken, LspDiagnostic, LspSession, SessionError, TextDocumentContentChangeEvent,
};
