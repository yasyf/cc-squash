//! ccs-core — the shared, zero-logic leaf of the cc-squash engine.
//!
//! Branded newtypes (`ModelId`, `RefId`, `MessageId`, and the token/byte/
//! generation units), the closed tag enums (`SegmentKind`, `ChoiceTag`),
//! `LineRange`, and the pure char-proxy token estimator. Everything here is
//! deterministic and I/O-free, so `ccs-economics`, `ccs-policy`, `ccs-refs`, and
//! `ccs-transcript` can share these names without depending on the whole engine.

pub mod estimate;
pub mod ids;
pub mod kind;
pub mod range;
pub mod units;

pub use estimate::{estimate_chars_proxy, CharProxyEstimator, TokenEstimator, TokenScale};
pub use ids::{MessageId, ModelId, RefId, RefIdError, SessionId};
pub use kind::{ChoiceTag, SegmentKind};
pub use range::LineRange;
pub use units::{ByteOffset, Generation, TokenCount};
