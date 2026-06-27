//! The preset pipelines and the per-request dispatch between them — the bioqa
//! `Presets.for_query` analog. [`Presets::continuous`] is the on-path squash spine,
//! [`Presets::budget_fallback`] the off-path over-budget ladder, and
//! [`Presets::for_request`] dispatches between them and the identity pipeline exactly
//! as `intercept.rs` does today.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;

use crate::budget::Pressure;
use crate::config::PolicyConfig;
use crate::pipeline::passes::{
    AnsiStripPass, AntiThrashPass, BlobExtractPass, DedupBackrefPass, DropOldestPass,
    DropToolPairsPass, EconomicsGatePass, HeadTailPass, JsonMinifyPass, JsonToonPass,
    LadderSelectPass, SalienceGatePass, ScorePass, StripReasoningPass, WhitespacePass,
};
use crate::pipeline::{Pipeline, Stage};

fn stage(pass: impl crate::pipeline::Pass + 'static) -> Stage {
    Stage::Pass(Arc::new(pass))
}

/// The named pipeline constructors. Each returns the pipeline the proxy runs in a
/// given regime; [`Presets::for_request`] dispatches between them.
pub struct Presets;

impl Presets {
    /// The continuous on-path squash spine: salience gate → score → ladder-select →
    /// economics gate → anti-thrash. The ladder/economics split + `hot_refs` filter
    /// decide per-segment; the surviving proposals feed the unchanged
    /// `Controller::decide` + apply seam.
    pub fn continuous(_knobs: &PolicyConfig) -> Pipeline {
        stage(SalienceGatePass)
            >> stage(ScorePass)
            >> stage(LadderSelectPass)
            >> stage(EconomicsGatePass)
            >> stage(AntiThrashPass)
    }

    /// The deterministic (non-LLM) recode chain, run OFF-PATH during staging: F
    /// blob-extract → D ANSI-strip → E whitespace-normalize → A JSON-minify → B JSON→TOON
    /// → C dedup-backref → J head/tail-truncate. Each pass refines the prior's `Recode`
    /// content (threaded through the ledger by
    /// [`recode_leaf`](crate::pipeline::passes::recode::recode_leaf)), so a single leaf is
    /// progressively cleaned, then ref-encoded, in order. Every pass is `Phase::OffPath`:
    /// the chain never runs on the 50ms L2 path in Phase 3.
    pub fn deterministic(_knobs: &PolicyConfig) -> Pipeline {
        stage(BlobExtractPass)
            >> stage(AnsiStripPass)
            >> stage(WhitespacePass)
            >> stage(JsonMinifyPass)
            >> stage(JsonToonPass)
            >> stage(DedupBackrefPass)
            >> stage(HeadTailPass)
    }

    /// The hard-ladder fallback pipeline, run when a turn is over budget: strip
    /// reasoning → drop tool pairs → drop oldest — the three over-budget rungs
    /// `intercept.rs::deterministic_compact` applies.
    pub fn budget_fallback(_knobs: &PolicyConfig) -> Pipeline {
        stage(StripReasoningPass) >> stage(DropToolPairsPass) >> stage(DropOldestPass)
    }

    /// Dispatch to the pipeline for this request, matching `intercept.rs`'s today:
    /// staged decisions present → the continuous spine; otherwise the budget fallback
    /// under [`Pressure::OverBudget`]; otherwise the identity pipeline.
    pub fn for_request(staged_present: bool, pressure: Pressure, knobs: &PolicyConfig) -> Pipeline {
        match (staged_present, pressure) {
            (true, _) => Self::continuous(knobs),
            (false, Pressure::OverBudget) => Self::budget_fallback(knobs),
            (false, Pressure::Nominal) => Pipeline::of([]),
        }
    }
}
