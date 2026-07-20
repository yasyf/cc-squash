//! The immutable identity records and the typed error of the store. A
//! [`RefRecord`] holds only the frozen identity+shape of a stored original; the
//! mutable accounting (`last_access_at`, `access_count`, `pinned`) lives solely
//! in SQLite columns, so a record never drifts from what was stored.

use ccs_core::{MessageId, RefId, SegmentKind, SessionId, TokenCount};
use serde::{Deserialize, Serialize};

/// The frozen identity and shape of a stored original.
///
/// Mutable accounting (`last_access_at`, `access_count`, `pinned`) lives only as
/// SQLite columns, never on this record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefRecord {
    /// The content-address of the stored original.
    pub ref_id: RefId,
    /// The byte length of the original — drives the lazy `st_size` path.
    pub byte_len: u64,
    /// The token estimate computed once at `put` via the char proxy.
    pub token_estimate: TokenCount,
    /// The message the original was extracted from.
    pub source_uuid: MessageId,
    /// The session that owns this ref (its GC/persistence scope).
    pub session_id: SessionId,
    /// The kind of segment the original was.
    pub kind: SegmentKind,
    /// The wall-clock time the ref was first stored, as unix seconds.
    pub created_at: f64,
}

/// A materialized original plus the accounting the scorer reads back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Materialized {
    /// The content-address that was materialized.
    pub ref_id: RefId,
    /// The full original text.
    pub text: String,
    /// The token estimate stored at `put`.
    pub token_estimate: TokenCount,
    /// The post-bump access count — the scorer's anti-thrash signal.
    pub access_count: u64,
}

/// The outcome of a `retrieve`: either the (optionally searched-within) text or
/// a miss the caller renders as the recovery hint.
#[derive(Debug, Clone, PartialEq)]
pub enum RetrieveResult {
    /// The ref was found; `text` is the full or BM25-searched original.
    Hit {
        /// The original text, or the top BM25 passages when a query was given.
        text: String,
        /// The post-bump access count.
        access_count: u64,
    },
    /// The ref was not stored — the caller renders [`crate::RECOVERY_HINT`].
    Miss,
}

/// An error from the reversible store.
#[derive(Debug, thiserror::Error)]
pub enum RefError {
    /// A local filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A database error from the underlying sqlite connection.
    #[error(transparent)]
    Db(#[from] tokio_rusqlite::Error),
    /// The database is not the exact epoch-1 schema.
    #[error("refs store schema mismatch: {0}")]
    Schema(String),
}
