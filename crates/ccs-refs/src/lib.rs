//! ccs-refs — the content-addressed reversible store (Layer 3).
//!
//! Every squash is recoverable: the original bytes are content-addressed into a
//! `sha256:<64hex>` [`ccs_core::RefId`] and the live wire carries a placeholder
//! pointing back at the stored original (resolved by the model via the
//! `retrieve` tool, or the optional FUSE path in Layer 6).
//!
//! - [`hash::content_address`] — the SOLE place bytes become a digest.
//! - [`record`] — `RefRecord` / `Materialized` / `RetrieveResult` / `RefError`.
//! - [`store::RefStore`] — the single tokio-rusqlite actor (sole writer + sole Rust reader).
//! - [`marker`] — `REF_MARKER` regex, `extract_refs`, placeholder + backref renderers.
//! - [`dedup`] — §3d dedup gates; `dedupe_key` is the content-address.
//! - [`bm25`] — hand-rolled BM25 search-within for `retrieve`.
//!
//! `RefStore` is the sole writer and `materialize` the sole Rust reader; once the
//! Layer-6 Go RO-CAS host reads `refs.db`, the schema is additive-only.

pub mod bm25;
pub mod dedup;
pub mod hash;
pub mod marker;
pub mod record;
pub mod store;

pub use dedup::{can_dedupe_from, dedupe_key, should_dedupe};
pub use hash::content_address;
pub use marker::{extract_refs, render_backref, render_placeholder, RECOVERY_HINT};
pub use record::{Materialized, RefError, RefRecord, RetrieveResult};
pub use store::RefStore;
