//! The concrete passes. The on-path squash spine (salience gate → score →
//! ladder-select → economics gate → anti-thrash), the off-path budget-fallback
//! ladder (strip-reasoning → drop-tool-pairs → drop-oldest), and the on-path
//! inline-lossless fast-lane helpers ([`fast_lane`]). Composed into pipelines by
//! [`Presets`](crate::pipeline::presets::Presets).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod ansi_strip;
pub mod anti_thrash;
pub mod blob_extract;
pub mod budget_fallback;
pub mod dedup_backref;
pub mod fast_lane;
pub mod head_tail;
pub mod json_minify;
pub mod json_toon;
pub mod ladder_select;
pub mod markup_strip;
pub mod recode;
pub mod salience_gate;
pub mod score;
pub mod seq_diff;
pub mod whitespace;

pub use ansi_strip::AnsiStripPass;
pub use anti_thrash::AntiThrashPass;
pub use blob_extract::BlobExtractPass;
pub use budget_fallback::{DropOldestPass, DropToolPairsPass, StripReasoningPass};
pub use dedup_backref::DedupBackrefPass;
pub use fast_lane::{fast_lane_clean, fast_lane_leaf};
pub use head_tail::HeadTailPass;
pub use json_minify::JsonMinifyPass;
pub use json_toon::JsonToonPass;
pub use ladder_select::{EconomicsGatePass, LadderSelectPass};
pub use markup_strip::MarkupStripPass;
pub use salience_gate::SalienceGatePass;
pub use score::ScorePass;
pub use seq_diff::SeqDiffPass;
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
