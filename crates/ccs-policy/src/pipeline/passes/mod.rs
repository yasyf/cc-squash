//! The concrete passes. The on-path squash spine (salience gate → score →
//! ladder-select → economics gate → anti-thrash) and the off-path budget-fallback
//! ladder (strip-reasoning → drop-tool-pairs → drop-oldest). Composed into pipelines by
//! [`Presets`](crate::pipeline::presets::Presets).
//!
//! DEFERRED passes (roadmap, not yet implemented): G sequential diff-encoding,
//! I markdown/HTML strip, and the on-path inline-lossless fast-lane (render
//! lossless recodes without a ref marker).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod ansi_strip;
pub mod anti_thrash;
pub mod blob_extract;
pub mod budget_fallback;
pub mod dedup_backref;
pub mod head_tail;
pub mod json_minify;
pub mod json_toon;
pub mod ladder_select;
pub mod recode;
pub mod salience_gate;
pub mod score;
pub mod whitespace;

pub use ansi_strip::AnsiStripPass;
pub use anti_thrash::AntiThrashPass;
pub use blob_extract::BlobExtractPass;
pub use budget_fallback::{DropOldestPass, DropToolPairsPass, StripReasoningPass};
pub use dedup_backref::DedupBackrefPass;
pub use head_tail::HeadTailPass;
pub use json_minify::JsonMinifyPass;
pub use json_toon::JsonToonPass;
pub use ladder_select::{EconomicsGatePass, LadderSelectPass};
pub use salience_gate::SalienceGatePass;
pub use score::ScorePass;
pub use whitespace::WhitespacePass;

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::pipeline::pass::{
        Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Provenance,
    };

    /// A no-op pass that only records its own provenance — exercises the Runner
    /// without proposing any rewrite.
    pub(crate) struct IdentityPass {
        pub id: PassId,
        pub phase: Phase,
    }

    impl Pass for IdentityPass {
        fn id(&self) -> PassId {
            self.id
        }

        fn phase(&self) -> Phase {
            self.phase
        }

        fn apply(&self, _ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
            ledger.record(Provenance {
                seg_index: 0,
                by: self.id,
                note: "identity",
            });
            PassControl::Continue
        }
    }
}
