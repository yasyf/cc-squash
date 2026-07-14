//! The declarative compaction pipeline: a flat sequence of [`Stage`]s (passes and
//! checkpoints) the [`Runner`] folds over a [`PlanLedger`]. [`Pipeline::of`] flattens
//! nested pipelines and `Stage >> Stage` composes them, so a pipeline reads like the
//! bioqa `Stage >> Stage` DSL. The runner is sync: it iterates stages, records
//! checkpoint positions, and on [`PassControl::RevertTo`] rewinds to that checkpoint
//! — capped at [`Runner::max_reverts`] so a cycling pass can never loop forever.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;

pub mod pass;
pub mod passes;
pub mod presets;
pub mod scorer;

pub use pass::{
    Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Proposal, Provenance, StagedDecisions,
    StagedSegment,
};
pub use presets::Presets;
pub use scorer::{score_segment, ScoreTable, ScoreWeights, SegmentScore};

/// The default cap on pipeline reverts — how many times the runner may honor a
/// [`PassControl::RevertTo`] before it stops rewinding and runs straight through.
pub const DEFAULT_MAX_REVERTS: usize = 2;

/// A named rewind point in a [`Pipeline`]. A [`PassControl::RevertTo`] carrying this
/// id jumps the runner back to the matching [`Stage::Checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CheckpointId(pub &'static str);

/// One step of a [`Pipeline`]: a pass to run, or a checkpoint to rewind to. The pass
/// is held in an [`Arc`] so [`Pipeline::on_path`] can share it into a filtered
/// pipeline without re-boxing — a pass is stateless, so sharing is free.
#[derive(Clone)]
pub enum Stage {
    Pass(Arc<dyn Pass>),
    Checkpoint(CheckpointId),
}

/// A flat, ordered sequence of [`Stage`]s. Compose with [`Pipeline::of`] (which
/// flattens nested pipelines) or the `>>` operator on [`Stage`]s.
#[derive(Default)]
pub struct Pipeline {
    stages: Vec<Stage>,
}

impl Pipeline {
    /// Build a pipeline from stages, FLATTENING any nested pipeline a stage came from.
    pub fn of(parts: impl IntoIterator<Item = Stage>) -> Pipeline {
        Pipeline {
            stages: parts.into_iter().collect(),
        }
    }

    /// The pipeline keeping only [`Phase::OnPath`] passes (and every checkpoint), for
    /// the synchronous 50ms L2 path.
    pub fn on_path(&self) -> Pipeline {
        Pipeline {
            stages: self
                .stages
                .iter()
                .filter(|s| match s {
                    Stage::Pass(p) => p.phase() == Phase::OnPath,
                    Stage::Checkpoint(_) => true,
                })
                .cloned()
                .collect(),
        }
    }

    /// The stages, in declared order.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Append `other`'s stages onto this pipeline, flattening it.
    fn extend(mut self, other: Pipeline) -> Pipeline {
        self.stages.extend(other.stages);
        self
    }
}

impl std::ops::Shr<Stage> for Stage {
    type Output = Pipeline;

    fn shr(self, rhs: Stage) -> Pipeline {
        Pipeline::of([self, rhs])
    }
}

impl std::ops::Shr<Pipeline> for Stage {
    type Output = Pipeline;

    fn shr(self, rhs: Pipeline) -> Pipeline {
        Pipeline::of([self]).extend(rhs)
    }
}

impl std::ops::Shr<Stage> for Pipeline {
    type Output = Pipeline;

    fn shr(self, rhs: Stage) -> Pipeline {
        self.extend(Pipeline::of([rhs]))
    }
}

impl std::ops::Shr<Pipeline> for Pipeline {
    type Output = Pipeline;

    fn shr(self, rhs: Pipeline) -> Pipeline {
        self.extend(rhs)
    }
}

/// Runs a [`Pipeline`] over a [`PlanLedger`], honoring [`PassControl::RevertTo`] up to
/// [`Runner::max_reverts`] times. Sync and unwind-friendly: it owns no state beyond
/// the revert budget.
#[derive(Debug, Clone, Copy)]
pub struct Runner {
    pub max_reverts: usize,
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            max_reverts: DEFAULT_MAX_REVERTS,
        }
    }
}

impl Runner {
    /// Fold `pipeline` over `ledger`, running each [`Stage::Pass`] against `ctx` and
    /// recording each [`Stage::Checkpoint`] position. A [`PassControl::RevertTo`] jumps
    /// back to the matching checkpoint; once the [`Runner::max_reverts`] budget is
    /// spent, further reverts are ignored and the runner continues straight through.
    pub fn run(&self, pipeline: &Pipeline, ctx: &PassCtx, ledger: &mut PlanLedger) {
        let stages = pipeline.stages();
        let mut reverts = 0usize;
        let mut i = 0usize;
        while i < stages.len() {
            match &stages[i] {
                Stage::Checkpoint(_) => i += 1,
                Stage::Pass(pass) => match pass.apply(ctx, ledger) {
                    PassControl::Continue => i += 1,
                    PassControl::RevertTo(id) if reverts < self.max_reverts => {
                        reverts += 1;
                        i = checkpoint_position(stages, id).map_or(i + 1, |pos| pos + 1);
                    }
                    PassControl::RevertTo(_) => i += 1,
                },
            }
        }
    }
}

fn checkpoint_position(stages: &[Stage], id: CheckpointId) -> Option<usize> {
    stages
        .iter()
        .position(|s| matches!(s, Stage::Checkpoint(cid) if *cid == id))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ccs_core::{ByteOffset, Generation, ModelId, SegmentKind, TokenCount};
    use ccs_economics::{CacheState, ModelEconomics};

    use super::*;
    use crate::config::PolicyConfig;
    use crate::pipeline::pass::StagedDecisions;
    use crate::pipeline::passes::fixtures::IdentityPass;
    use crate::salience::WorkingState;
    use crate::segment::Segment;
    use crate::wire::parse_body;

    const CP: CheckpointId = CheckpointId("cp");

    fn boxed(id: &'static str, phase: Phase) -> Stage {
        Stage::Pass(Arc::new(IdentityPass {
            id: PassId(id),
            phase,
        }))
    }

    fn body() -> Vec<u8> {
        br#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}],"max_tokens":256}"#
            .to_vec()
    }

    fn cache() -> CacheState {
        CacheState {
            cached_prefix_tokens: TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: ModelId::new("claude-opus-4-8"),
            breakpoints: vec![],
        }
    }

    fn econ() -> ModelEconomics {
        ModelEconomics {
            base_input: 5e-6,
            write_mult: 2.0,
            read_mult: 0.1,
            min_cache_floor: TokenCount(1024),
        }
    }

    fn seg() -> Segment {
        Segment {
            index: 0,
            kind: SegmentKind::UserTurn,
            byte_offset: ByteOffset(0),
            token_estimate: TokenCount(1),
            generation: Generation(1),
            pinned: false,
            is_current: true,
            is_true_human: true,
            source_uuids: vec![],
        }
    }

    struct Fixtures {
        body_bytes: Vec<u8>,
        segments: Vec<Segment>,
        working: WorkingState,
        cache: CacheState,
        econ: ModelEconomics,
        knobs: PolicyConfig,
        staged: StagedDecisions,
    }

    impl Default for Fixtures {
        fn default() -> Self {
            Self {
                body_bytes: body(),
                segments: vec![seg()],
                working: WorkingState::default(),
                cache: cache(),
                econ: econ(),
                knobs: PolicyConfig::default(),
                staged: StagedDecisions::default(),
            }
        }
    }

    impl Fixtures {
        fn run(&self, pipeline: &Pipeline, runner: Runner) -> PlanLedger {
            let parsed = parse_body(&self.body_bytes).expect("parse");
            let ctx = PassCtx {
                body: &parsed,
                segments: &self.segments,
                working: &self.working,
                econ: &self.econ,
                cache: &self.cache,
                knobs: &self.knobs,
                staged: &self.staged,
                remaining_turns: 10.0,
                now: 0.0,
            };
            let mut ledger = PlanLedger::sized(self.segments.len());
            runner.run(pipeline, &ctx, &mut ledger);
            ledger
        }
    }

    #[test]
    fn of_flattens_nested_pipelines() {
        let nested = boxed("a", Phase::OnPath) >> boxed("b", Phase::OnPath);
        let outer =
            Pipeline::of([boxed("x", Phase::OnPath)]) >> nested >> boxed("y", Phase::OnPath);
        assert_eq!(outer.stages().len(), 4);
    }

    #[test]
    fn on_path_drops_off_path_passes() {
        let pipeline = boxed("on", Phase::OnPath)
            >> Stage::Checkpoint(CP)
            >> boxed("off", Phase::OffPath)
            >> boxed("on2", Phase::OnPath);
        let filtered = pipeline.on_path();
        let names: Vec<_> = filtered
            .stages()
            .iter()
            .filter_map(|s| match s {
                Stage::Pass(p) => Some(p.id().0),
                Stage::Checkpoint(_) => None,
            })
            .collect();
        assert_eq!(names, vec!["on", "on2"]);
        assert_eq!(
            filtered
                .stages()
                .iter()
                .filter(|s| matches!(s, Stage::Checkpoint(_)))
                .count(),
            1
        );
    }

    #[test]
    fn runner_runs_passes_in_order_and_mutates_ledger() {
        let pipeline =
            boxed("a", Phase::OnPath) >> boxed("b", Phase::OnPath) >> boxed("c", Phase::OnPath);
        let ledger = Fixtures::default().run(&pipeline, Runner::default());
        let order: Vec<_> = ledger.provenance.iter().map(|p| p.by.0).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn revert_to_loops_back_and_is_capped() {
        static HITS: AtomicUsize = AtomicUsize::new(0);

        struct Reverting;
        impl Pass for Reverting {
            fn id(&self) -> PassId {
                PassId("reverting")
            }
            fn phase(&self) -> Phase {
                Phase::OnPath
            }
            fn apply(&self, _ctx: &PassCtx, _ledger: &mut PlanLedger) -> PassControl {
                HITS.fetch_add(1, Ordering::SeqCst);
                PassControl::RevertTo(CP)
            }
        }

        let pipeline = Stage::Checkpoint(CP) >> Stage::Pass(Arc::new(Reverting));
        Fixtures::default().run(&pipeline, Runner { max_reverts: 2 });
        // The pass runs once, each of the 2 honored reverts re-runs it, then the
        // 3rd revert is over budget and the loop terminates: 3 invocations.
        assert_eq!(HITS.load(Ordering::SeqCst), 3);
    }
}
