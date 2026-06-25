//! ccs-proxy — the cc-squash data plane.
//!
//! A streaming proxy at `ANTHROPIC_BASE_URL` that, on every `/v1/messages`
//! request, prices keep-vs-evict per context segment and rewrites the request
//! to minimise prompt-cache cost. Layer 1 is the RelayCore: a transparent,
//! fail-open passthrough plus the v0 `<summary>` synthesis capability. The
//! Go control plane (`ccs`) supervises this child over `proxy.sock`.
//!
//! Cardinal invariant: fail-open to identity. Any error/timeout/panic ⇒ forward
//! the original request and relay the original response byte-for-byte. Unlike the
//! rest of the repo, this crate does NOT "crash on the unexpected" on the hot
//! path — a relay that panics is a worse failure than a relay that passes
//! through unchanged (build plan §5/§9).

pub mod app;
pub mod config;
pub mod demux;
pub mod forward;
pub mod headers;
pub mod intercept;
pub mod mcp;
pub mod relay;
pub mod seam;
pub mod session;
pub mod staging;
pub mod synth;
pub mod usage_tap;

pub use app::{router, AppState};
pub use mcp::mcp_router;
