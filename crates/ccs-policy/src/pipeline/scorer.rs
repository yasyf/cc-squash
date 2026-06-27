//! The explicit multi-signal segment scorer. [`score_segment`] is a pure function of
//! its inputs (`now` is passed, never read from a clock), so the same segment scores
//! identically every run — the determinism the [`ScoreTable`] (a dense `Vec` indexed
//! by `seg.index`, never a `HashMap`) preserves. The pin and recency-window signals
//! are HARD VETOES that force [`SegmentScore::value`] to `NEG_INFINITY`. The non-veto
//! signals fold into a clamped weighted sum under [`ScoreWeights`].
//!
//! Phase 4 lights the score up: [`quality_gain`] turns the scalar [`SegmentScore::value`]
//! into the `Q` term the NPV gate reads, and [`SegmentScore::admitted`] is the (default
//! off) score floor. The wiring is no-regression by construction — `Q >= 0` can only
//! raise a candidate's NPV, and the floor defaults to `NEG_INFINITY` (admit-all).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::SegmentKind;
use ccs_economics::{CacheState, ModelEconomics};

use crate::config::PolicyConfig;
use crate::salience::{is_pinned, WorkingState};
use crate::segment::{is_recency_protected, Segment};

/// The freshness decay time-constant in generations (Ebbinghaus `tau`): a segment one
/// `tau` older than the newest generation reaches `1 - 1/e ≈ 0.63`. Conservative — a
/// few full turns, matching the recency window's order of magnitude.
const FRESHNESS_TAU_GENS: f64 = 4.0;

/// The token estimate at which the size signal saturates to `1.0`.
const SIZE_SATURATION_TOKENS: f64 = 4_096.0;

/// Dollar-equivalent scale mapping the unit-scaled `value()` to the `Q` quality-gain
/// term. A modest default: at the calibrated opus rate (`base_input · read_mult ≈
/// 5e-7`/token/turn) this is the recurring saving of roughly a thousand removed tokens
/// over one turn — enough to break a tie in NPV, far too small to flood the gate.
const Q_WEIGHT_DOLLARS: f64 = 5e-4;

/// The per-signal scores for one segment, each in `[0, 1]` except the two vetoes.
///
/// `salience` and `recency` are HARD VETOES: a `>= 1.0` value forces
/// [`SegmentScore::value`] to `f64::NEG_INFINITY`, so a pinned or recency-protected
/// segment can never be selected for rewrite. The remaining signals fold into a
/// clamped weighted sum, with `access_count` entering as a penalty (subtractive).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentScore {
    pub salience: f64,
    pub recency: f64,
    pub freshness: f64,
    pub economics: f64,
    pub size: f64,
    pub content_type: f64,
    pub access_count: f64,
}

impl Default for SegmentScore {
    fn default() -> Self {
        Self {
            salience: 0.0,
            recency: 0.0,
            freshness: 0.0,
            economics: 0.0,
            size: 0.0,
            content_type: 0.0,
            access_count: 0.0,
        }
    }
}

/// The relative weight of each non-veto signal in the scalar [`SegmentScore::value`],
/// plus the signal scales (`tau`, `size_scale`) and the two decision knobs (`q_weight`,
/// `score_floor`). Knob-driven via [`PolicyConfig`]; the [`Default`] is conservative —
/// equal unit weights, a modest `q_weight` that nudges NPV without flooding it, and a
/// `score_floor` of `NEG_INFINITY` so the admission floor removes nothing (off by
/// default, on as a future-conservatism knob).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreWeights {
    pub freshness: f64,
    pub economics: f64,
    pub size: f64,
    pub content_type: f64,
    pub access_count: f64,
    /// Freshness decay time-constant in generations (Ebbinghaus `tau`): a segment one
    /// `tau` older than the newest generation reaches `1 - 1/e ≈ 0.63`.
    pub tau: f64,
    /// The token estimate at which the size signal saturates toward `1.0`.
    pub size_scale: f64,
    /// Dollar-equivalent scale mapping `value()` to the `Q` quality-gain term. Modest by
    /// default so the score nudges admission rather than dominating the NPV.
    pub q_weight: f64,
    /// The `value()` below which [`SegmentScore::admitted`] returns `false`. Defaults to
    /// `NEG_INFINITY` (admit-all): the floor is a future-conservatism knob, off by default.
    pub score_floor: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            freshness: 1.0,
            economics: 1.0,
            size: 1.0,
            content_type: 1.0,
            access_count: 1.0,
            tau: FRESHNESS_TAU_GENS,
            size_scale: SIZE_SATURATION_TOKENS,
            q_weight: Q_WEIGHT_DOLLARS,
            score_floor: f64::NEG_INFINITY,
        }
    }
}

/// A dense table of [`SegmentScore`]s indexed by `seg.index`. A `Vec`, never a
/// `HashMap`: positional indexing keeps scoring deterministic and order-stable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScoreTable {
    scores: Vec<SegmentScore>,
}

impl ScoreTable {
    /// A table sized to `count` segments, every slot at the default zero score.
    pub fn sized(count: usize) -> ScoreTable {
        ScoreTable {
            scores: vec![SegmentScore::default(); count],
        }
    }

    /// The score for `seg_index`, or `None` when the index is out of range.
    pub fn get(&self, seg_index: usize) -> Option<&SegmentScore> {
        self.scores.get(seg_index)
    }

    /// Set the score for `seg_index`. Panics only on an out-of-range index, an
    /// impossible state once the table is sized to the segment count.
    pub fn set(&mut self, seg_index: usize, score: SegmentScore) {
        self.scores[seg_index] = score;
    }

    /// The number of scored slots.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    /// Whether the table holds no slots.
    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }
}

impl SegmentScore {
    /// The scalar value of this score under `w`. Either veto (`salience >= 1.0` or
    /// `recency >= 1.0`) yields `f64::NEG_INFINITY`; otherwise a clamped weighted sum
    /// of the non-veto signals, with `access_count` subtracted as a penalty.
    pub fn value(&self, w: &ScoreWeights) -> f64 {
        if self.salience >= 1.0 || self.recency >= 1.0 {
            return f64::NEG_INFINITY;
        }
        (w.freshness * clamp_unit(self.freshness)
            + w.economics * clamp_unit(self.economics)
            + w.size * clamp_unit(self.size)
            + w.content_type * clamp_unit(self.content_type)
            - w.access_count * clamp_unit(self.access_count))
        .max(0.0)
    }

    /// The dollar-equivalent quality-gain `Q` this score contributes to a candidate's
    /// NPV: `q_weight · value()`, floored at `0.0`. A vetoed score (`value` is
    /// `NEG_INFINITY`) contributes `0` — a vetoed segment never reaches a candidate, but
    /// the floor keeps `Q >= 0` unconditional, so lighting the score up can only RAISE a
    /// candidate's NPV. Never returns a negative or non-finite `Q`.
    pub fn quality_gain(&self, w: &ScoreWeights) -> f64 {
        match w.q_weight * self.value(w) {
            q if q.is_finite() => q.max(0.0),
            _ => 0.0,
        }
    }

    /// Whether this score clears the admission floor `w.score_floor`. The default floor
    /// is `NEG_INFINITY` (admit-all), so this removes nothing vs baseline; a finite floor
    /// is the future-conservatism knob. A vetoed score (`value` is `NEG_INFINITY`) is
    /// never admitted unless the floor is also `NEG_INFINITY`.
    pub fn admitted(&self, w: &ScoreWeights) -> bool {
        self.value(w) >= w.score_floor
    }
}

/// Score one segment against the live working/cache/economics state at `now`.
///
/// Pure and deterministic: identical inputs yield an identical [`SegmentScore`].
/// The veto signals reuse [`is_pinned`] and [`is_recency_protected`], so a segment
/// the engine must keep verbatim scores `salience`/`recency` `1.0` and is forced to
/// `NEG_INFINITY` by [`SegmentScore::value`]. The non-veto signals read their scales
/// from `knobs.weights` (`tau`, `size_scale`). `access_count` is fed from the staged
/// hot-ref snapshot by the [`ScorePass`](crate::pipeline::passes::ScorePass), not here —
/// the pure scorer has no ref-liveness view, so it scores the segment's intrinsic
/// signals and leaves `access_count` at `0.0` for the pass to overwrite.
#[allow(clippy::too_many_arguments)] // the score's inputs are irreducible: segment, segments, working, cache, economics, now, policy.
pub fn score_segment(
    seg: &Segment,
    segments: &[Segment],
    working: &WorkingState,
    cache: &CacheState,
    econ: &ModelEconomics,
    now: f64,
    knobs: &PolicyConfig,
) -> SegmentScore {
    SegmentScore {
        salience: veto(is_pinned(seg, working)),
        recency: veto(is_recency_protected(seg, segments, knobs)),
        freshness: freshness_signal(seg, segments, &knobs.weights),
        economics: economics_signal(seg, cache, econ, now),
        size: size_signal(seg, &knobs.weights),
        content_type: content_type_signal(seg),
        access_count: 0.0,
    }
}

fn veto(flag: bool) -> f64 {
    match flag {
        true => 1.0,
        false => 0.0,
    }
}

fn clamp_unit(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Ebbinghaus decay over the generation ordinal: `1 - exp(-gap / tau)`, where `gap` is
/// how many generations older than the newest the segment is. Older ⇒ higher ⇒ more
/// squashable; the newest generation scores `0`, monotone up toward `1`.
fn freshness_signal(seg: &Segment, segments: &[Segment], w: &ScoreWeights) -> f64 {
    let newest = segments
        .iter()
        .map(|s| s.generation.get())
        .max()
        .unwrap_or(0);
    let gap = f64::from(newest.saturating_sub(seg.generation.get()));
    clamp_unit(1.0 - (-gap / w.tau).exp())
}

/// The single-candidate NPV signal, mapped to `[0, 1]`. The cost model already bakes in
/// position (`suffix_tokens`) and bust risk (`p_alive`); here the scorer has no
/// candidate, so it prices the segment's own removable mass over one turn against the
/// warm-cache bust on its suffix and squashes the dollar result through a logistic.
/// Pinned segments contribute `0` (never squashed for economics).
fn economics_signal(seg: &Segment, cache: &CacheState, econ: &ModelEconomics, now: f64) -> f64 {
    if seg.pinned {
        return 0.0;
    }
    let removed = f64::from(seg.token_estimate.get());
    let saving = removed * econ.base_input * econ.read_mult;
    let bust = removed * econ.base_input * (econ.write_mult - econ.read_mult) * cache.p_alive(now);
    logistic((saving - bust) / saving.max(f64::MIN_POSITIVE))
}

fn size_signal(seg: &Segment, w: &ScoreWeights) -> f64 {
    clamp_unit(f64::from(seg.token_estimate.get()) / w.size_scale)
}

fn content_type_signal(seg: &Segment) -> f64 {
    match seg.kind {
        SegmentKind::ToolPair => 1.0,
        SegmentKind::AssistantTurn => 0.6,
        SegmentKind::System | SegmentKind::Tools => 0.3,
        SegmentKind::UserTurn => 0.1,
    }
}

/// The standard logistic squashing `x` into `(0, 1)`.
fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

#[cfg(test)]
mod tests {
    use ccs_core::{ByteOffset, Generation, ModelId, TokenCount};
    use proptest::prelude::*;

    use super::*;

    fn seg(index: usize, kind: SegmentKind, token_estimate: u32, pinned: bool) -> Segment {
        Segment {
            index,
            kind,
            byte_offset: ByteOffset(0),
            token_estimate: TokenCount(token_estimate),
            generation: Generation(1),
            pinned,
            is_current: false,
            is_true_human: false,
            source_uuids: vec![],
        }
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

    #[test]
    fn pinned_segment_value_is_neg_infinity() {
        let score = score_segment(
            &seg(0, SegmentKind::UserTurn, 100, true),
            &[seg(0, SegmentKind::UserTurn, 100, true)],
            &WorkingState::default(),
            &cache(),
            &econ(),
            0.0,
            &PolicyConfig::default(),
        );
        assert_eq!(score.value(&ScoreWeights::default()), f64::NEG_INFINITY);
    }

    #[test]
    fn recency_protected_segment_value_is_neg_infinity() {
        let segments = vec![
            seg(0, SegmentKind::ToolPair, 100, false),
            seg(1, SegmentKind::ToolPair, 100, false),
        ];
        let score = score_segment(
            &segments[1],
            &segments,
            &WorkingState::default(),
            &cache(),
            &econ(),
            0.0,
            &PolicyConfig::default(),
        );
        assert_eq!(score.recency, 1.0);
        assert_eq!(score.value(&ScoreWeights::default()), f64::NEG_INFINITY);
    }

    #[test]
    fn value_is_finite_and_non_negative_for_unvetoed() {
        let head = seg(0, SegmentKind::ToolPair, 8_192, false);
        let segments = vec![
            head.clone(),
            seg(1, SegmentKind::ToolPair, 100, false),
            seg(2, SegmentKind::ToolPair, 100, false),
            seg(3, SegmentKind::ToolPair, 100, false),
        ];
        let v = score_segment(
            &segments[0],
            &segments,
            &WorkingState::default(),
            &cache(),
            &econ(),
            0.0,
            &PolicyConfig::default(),
        )
        .value(&ScoreWeights::default());
        assert!(v.is_finite());
        assert!(v >= 0.0);
    }

    fn kind_any() -> impl Strategy<Value = SegmentKind> {
        prop_oneof![
            Just(SegmentKind::UserTurn),
            Just(SegmentKind::AssistantTurn),
            Just(SegmentKind::ToolPair),
            Just(SegmentKind::System),
            Just(SegmentKind::Tools),
        ]
    }

    fn weights_any() -> impl Strategy<Value = ScoreWeights> {
        (
            0.0f64..3.0,
            0.0f64..3.0,
            0.0f64..3.0,
            0.0f64..3.0,
            0.0f64..3.0,
            0.5f64..16.0,
            256.0f64..16_384.0,
            0.0f64..1.0,
            -1.0f64..3.0,
        )
            .prop_map(
                |(
                    freshness,
                    economics,
                    size,
                    content_type,
                    access_count,
                    tau,
                    size_scale,
                    q_weight,
                    score_floor,
                )| ScoreWeights {
                    freshness,
                    economics,
                    size,
                    content_type,
                    access_count,
                    tau,
                    size_scale,
                    q_weight,
                    score_floor,
                },
            )
    }

    proptest! {
        /// Purity: `score_segment`, and the derived `value`/`quality_gain`/`admitted`, are
        /// a pure function of (segment, segments, working, cache, econ, now, knobs). Two
        /// runs over identical inputs yield identical results — no clock, RNG, or
        /// HashMap-iteration nondeterminism. Varying the weights (and `access_count`) does
        /// not perturb the *score* itself, only its scalar reductions.
        #[test]
        fn score_segment_is_deterministic(
            kind in kind_any(),
            token_estimate in 0u32..20_000,
            gen in 0u32..20,
            generations in 0u32..20,
            pinned in any::<bool>(),
            is_true_human in any::<bool>(),
            now in 0.0f64..10_000.0,
            w in weights_any(),
            access_count in 0.0f64..1.0,
        ) {
            let mut target = seg(2, kind, token_estimate, pinned);
            target.generation = Generation(gen);
            target.is_true_human = is_true_human;
            let segments = vec![
                {
                    let mut s = seg(0, SegmentKind::AssistantTurn, 50, false);
                    s.generation = Generation(generations);
                    s
                },
                seg(1, SegmentKind::ToolPair, 50, false),
                target.clone(),
                seg(3, SegmentKind::AssistantTurn, 50, false),
                seg(4, SegmentKind::UserTurn, 50, false),
            ];
            let working = WorkingState::default();
            let cache = cache();
            let econ = econ();
            let knobs = PolicyConfig {
                weights: w,
                ..PolicyConfig::default()
            };
            let mut first = score_segment(&segments[2], &segments, &working, &cache, &econ, now, &knobs);
            let mut second = score_segment(&segments[2], &segments, &working, &cache, &econ, now, &knobs);
            prop_assert_eq!(first, second);
            first.access_count = access_count;
            second.access_count = access_count;
            prop_assert_eq!(first.value(&w), second.value(&w));
            prop_assert_eq!(first.quality_gain(&w), second.quality_gain(&w));
            prop_assert_eq!(first.admitted(&w), second.admitted(&w));

            // `Q` is never negative or non-finite — the no-regression invariant's bedrock.
            prop_assert!(first.quality_gain(&w) >= 0.0);
            prop_assert!(first.quality_gain(&w).is_finite());

            let vetoed = is_pinned(&segments[2], &working)
                || is_recency_protected(&segments[2], &segments, &knobs);
            if vetoed {
                prop_assert_eq!(first.value(&w), f64::NEG_INFINITY);
                prop_assert_eq!(first.quality_gain(&w), 0.0);
            } else {
                prop_assert!(first.value(&w).is_finite());
            }
        }

        /// Vetoes are absolute: a pinned or recency-protected segment forces
        /// `value() == NEG_INFINITY` regardless of every non-veto signal and weight.
        #[test]
        fn vetoes_force_neg_infinity_under_any_weights(
            freshness in 0.0f64..1.0,
            economics in 0.0f64..1.0,
            size in 0.0f64..1.0,
            content_type in 0.0f64..1.0,
            access_count in 0.0f64..1.0,
            salience_vetoed in any::<bool>(),
            w in weights_any(),
        ) {
            // Exactly one veto set high; every other signal arbitrary.
            let score = SegmentScore {
                salience: if salience_vetoed { 1.0 } else { 0.0 },
                recency: if salience_vetoed { 0.0 } else { 1.0 },
                freshness,
                economics,
                size,
                content_type,
                access_count,
            };
            prop_assert_eq!(score.value(&w), f64::NEG_INFINITY);
            prop_assert_eq!(score.quality_gain(&w), 0.0);
        }
    }

    #[test]
    fn freshness_is_monotone_in_age() {
        let segments = vec![
            {
                let mut s = seg(0, SegmentKind::ToolPair, 100, false);
                s.generation = Generation(10);
                s
            },
            {
                let mut s = seg(1, SegmentKind::ToolPair, 100, false);
                s.generation = Generation(5);
                s
            },
            {
                let mut s = seg(2, SegmentKind::ToolPair, 100, false);
                s.generation = Generation(10);
                s
            },
        ];
        let w = ScoreWeights::default();
        // The newest generation scores 0; an older one scores higher (more squashable).
        assert_eq!(freshness_signal(&segments[0], &segments, &w), 0.0);
        assert!(freshness_signal(&segments[1], &segments, &w) > 0.0);
    }

    #[test]
    fn quality_gain_scales_with_q_weight_and_is_zero_at_baseline() {
        let score = SegmentScore {
            salience: 0.0,
            recency: 0.0,
            freshness: 1.0,
            economics: 1.0,
            size: 1.0,
            content_type: 1.0,
            access_count: 0.0,
        };
        let baseline = ScoreWeights {
            q_weight: 0.0,
            ..ScoreWeights::default()
        };
        // q_weight = 0 → the baseline: Q is exactly 0, so NPV is unchanged vs Phase 3.
        assert_eq!(score.quality_gain(&baseline), 0.0);
        // A positive q_weight yields a positive Q proportional to value().
        let lit = ScoreWeights::default();
        let q = score.quality_gain(&lit);
        assert!(q > 0.0);
        assert!((q - lit.q_weight * score.value(&lit)).abs() < 1e-12);
    }

    #[test]
    fn admission_floor_defaults_to_admit_all() {
        let low = SegmentScore::default(); // value() == 0.0
                                           // Default floor is NEG_INFINITY: every finite score is admitted.
        assert!(low.admitted(&ScoreWeights::default()));
        // A finite floor above the score rejects it (the future-conservatism knob).
        let strict = ScoreWeights {
            score_floor: 0.5,
            ..ScoreWeights::default()
        };
        assert!(!low.admitted(&strict));
    }

    #[test]
    fn score_table_is_dense_and_indexed() {
        let mut table = ScoreTable::sized(3);
        assert_eq!(table.len(), 3);
        table.set(
            1,
            SegmentScore {
                size: 0.5,
                ..SegmentScore::default()
            },
        );
        assert_eq!(table.get(1).map(|s| s.size), Some(0.5));
        assert_eq!(table.get(0).map(|s| s.size), Some(0.0));
        assert_eq!(table.get(3), None);
    }
}
