//! Compaction-request detection and `<summary>` SSE synthesis.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod detect;
pub mod sse;

pub use detect::{detect, BriefInputs};
pub use sse::synth_response;

/// The forward-vs-synthesize outcome for one buffered request body. Closed by
/// construction: detection either yields synthesis inputs or it does not.
pub enum Decision {
    Synthesize(BriefInputs),
    Forward,
}

/// Decide how to answer a buffered `/v1/messages` body. Any detection miss — a
/// non-compaction request, malformed JSON, an out-of-range budget — maps to
/// [`Decision::Forward`], the fail-open default.
pub fn decide(body: &[u8]) -> Decision {
    match detect(body) {
        Some(inputs) => Decision::Synthesize(inputs),
        None => Decision::Forward,
    }
}
