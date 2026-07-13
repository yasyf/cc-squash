//! The [`Pass`] trait and the data it threads. A pass is sync and PURE: it reads the
//! borrowed [`PassCtx`] plus the earlier proposals/scores in the [`PlanLedger`] and
//! contributes ADDITIVELY — refining the per-segment best [`Proposal`] and recording
//! [`Provenance`]. A pass never emits wire bytes (the splice is a later single step)
//! and never does I/O: an LLM round-trip or a freshly-written ref is an *intention*
//! carried in the proposal, executed in `ccs-proxy` (Phase 2).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;

use ccs_core::RefId;
use ccs_economics::{CacheState, ModelEconomics};

use crate::candidate::SquashCandidate;
use crate::config::PolicyConfig;
use crate::decision::ContentDecision;
use crate::pipeline::scorer::ScoreTable;
use crate::pipeline::CheckpointId;
use crate::salience::WorkingState;
use crate::segment::Segment;
use crate::strategy::Strategy;
use crate::wire::WireBody;

/// Whether a pass may run on the synchronous 50ms L2 path, or only off-path on L1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    OnPath,
    OffPath,
}

/// A pass's stable identifier, used in [`Proposal::by`] and [`Provenance::by`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PassId(pub &'static str);

impl fmt::Display for PassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// What the [`Runner`](crate::pipeline::Runner) does after a pass returns: keep
/// going, or rewind to a recorded checkpoint (capped, to bound cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassControl {
    Continue,
    RevertTo(CheckpointId),
}

/// The current best rewrite proposal for one segment. Passes refine it in place via
/// [`PlanLedger::upsert_proposal`]; `ref_id` is an *intention* (the ref `ccs-refs`
/// will mint in Phase 2), never minted here.
///
/// `needs_ref` is the deterministic-recode intention: a ref-backed pass (TOON, dedup,
/// blob-extract, head/tail truncate) needs the original bytes stored so off-path staging can
/// `content_address` + `RefStore::put` them and resolve the ref. It carries the earliest
/// ref-backed pass's original when the chain has one — both inline and later ref-backed passes
/// forward it — else the proposing pass's own input; a chain with no ref-backed pass leaves it
/// `None`. The proxy stores bytes from the raw wire body, so these are advisory. Passes stay
/// pure — they never mint the ref or write the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub seg_index: usize,
    pub strategy: Strategy,
    pub ref_id: Option<RefId>,
    pub needs_ref: Option<Vec<u8>>,
    pub net_removed: i64,
    pub quality_gain: f64,
    pub by: PassId,
}

/// A note of which pass touched a segment and why — the bioqa `AuditedSet` analog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub seg_index: usize,
    pub by: PassId,
    pub note: &'static str,
}

/// The shared, additively-built plan the passes refine: the [`ScoreTable`], the
/// per-segment best [`Proposal`]s, and the [`Provenance`] trail. There is at most
/// one proposal per `seg_index`; a later pass replaces an earlier one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanLedger {
    pub scores: ScoreTable,
    pub proposals: Vec<Proposal>,
    pub provenance: Vec<Provenance>,
}

impl PlanLedger {
    /// A ledger whose [`ScoreTable`] is sized to `segment_count`, with no proposals.
    pub fn sized(segment_count: usize) -> PlanLedger {
        PlanLedger {
            scores: ScoreTable::sized(segment_count),
            proposals: Vec::new(),
            provenance: Vec::new(),
        }
    }

    /// The current proposal for `seg_index`, if any pass has contributed one.
    pub fn proposal_for(&self, seg_index: usize) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.seg_index == seg_index)
    }

    /// Insert or replace the proposal for its `seg_index` — one proposal per segment.
    pub fn upsert_proposal(&mut self, proposal: Proposal) {
        match self
            .proposals
            .iter_mut()
            .find(|p| p.seg_index == proposal.seg_index)
        {
            Some(slot) => *slot = proposal,
            None => self.proposals.push(proposal),
        }
    }

    /// Append a provenance note.
    pub fn record(&mut self, provenance: Provenance) {
        self.provenance.push(provenance);
    }
}

/// One segment's pre-staged decision: the matched [`ContentDecision`], the live
/// [`SquashCandidate`] the proxy built off-path, and the per-egress `npv_floor` the
/// [`EconomicsGatePass`](crate::pipeline::passes::EconomicsGatePass) reads. The proxy
/// assembles these by content address before the on-path pipeline runs, so the passes
/// stay pure (no ref minting, no I/O).
#[derive(Debug, Clone, PartialEq)]
pub struct StagedSegment {
    pub seg_index: usize,
    pub decision: ContentDecision,
    pub candidate: SquashCandidate,
    pub npv_floor: f64,
}

/// Pre-staged content decisions a [`Pass`] reads without doing I/O. A pure side
/// table the proxy fills before running the on-path pipeline; empty (`present:
/// false`, no entries) in Phase 1. `hot_refs` is the snapshot of in-flight refs the
/// [`AntiThrashPass`](crate::pipeline::passes::AntiThrashPass) drops proposals against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StagedDecisions {
    pub present: bool,
    pub segments: Vec<StagedSegment>,
    pub hot_refs: Vec<RefId>,
}

impl StagedDecisions {
    /// The staged entry for `seg_index`, if the proxy matched one by content address.
    pub fn segment(&self, seg_index: usize) -> Option<&StagedSegment> {
        self.segments.iter().find(|s| s.seg_index == seg_index)
    }

    /// Whether `ref_id` is in the hot-ref snapshot — an in-flight ref the on-path
    /// anti-thrash filter must not re-propose this turn.
    pub fn is_hot(&self, ref_id: &RefId) -> bool {
        self.hot_refs.contains(ref_id)
    }
}

/// The borrowed, read-only context every [`Pass`] sees. Holds the inputs a pass
/// plausibly needs without owning any of them, so the runner can re-run the
/// pipeline over the same borrows.
pub struct PassCtx<'b> {
    pub body: &'b WireBody<'b>,
    pub segments: &'b [Segment],
    pub working: &'b WorkingState,
    pub econ: &'b ModelEconomics,
    pub cache: &'b CacheState,
    pub knobs: &'b PolicyConfig,
    pub staged: &'b StagedDecisions,
    pub remaining_turns: f64,
    pub now: f64,
}

/// One refinement stage of the compaction pipeline. Sync, pure, and object-safe
/// (run as `Box<dyn Pass>`): it reads `ctx` and the earlier ledger state, contributes
/// additively to `ledger`, and returns whether to continue or rewind.
pub trait Pass: Send + Sync {
    fn id(&self) -> PassId;
    fn phase(&self) -> Phase;
    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl;
}
