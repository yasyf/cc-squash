//! Cache-breakpoint planning. At most four `cache_control` positions, each at the
//! end of a stable rewritten prefix within the lookback window; [`cap_cache_hints`]
//! drops the earliest first. Takes `min_floor` as a plain
//! [`TokenCount`](ccs_core::TokenCount) so planning never depends on economics.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::TokenCount;

use crate::config::PolicyConfig;
use crate::segment::{is_recency_protected, Segment};

/// The maximum number of `cache_control` breakpoints Anthropic honors — the API hard
/// max. [`PolicyConfig::cache_hint_cap`] may emit fewer, never more.
pub const CACHE_HINT_CAP: usize = 4;

/// How many block positions back from the tail the planner considers.
/// Tunable via [`PolicyConfig::lookback_positions`]; this is the default.
pub const LOOKBACK_POSITIONS: usize = 20;

/// A plan of `cache_control` block positions (at most [`CACHE_HINT_CAP`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakpointPlan {
    pub positions: Vec<usize>,
}

/// Plan breakpoints over the segments, keeping each prefix at least `min_floor`
/// tokens and each position within [`PolicyConfig::lookback_positions`] of the tail.
///
/// A position is the index of a segment at the end of a *stable* cacheable prefix:
/// it sits within the lookback window, ahead of the volatile recency tail
/// ([`is_recency_protected`]), and its cumulative `token_estimate` clears
/// `min_floor` — below the floor Anthropic silently disengages caching. When the
/// whole prefix is sub-floor (or the window holds no stable position) the plan is
/// empty. At most [`PolicyConfig::cache_hint_cap`] positions survive, the latest
/// kept — the validity gate independently enforces the [`CACHE_HINT_CAP`] hard max.
pub fn plan_breakpoints(
    segments: &[Segment],
    min_floor: TokenCount,
    cfg: &PolicyConfig,
) -> BreakpointPlan {
    let floor = u64::from(min_floor.get());
    let lookback_start = segments.len().saturating_sub(cfg.lookback_positions);
    let positions = segments
        .iter()
        .scan(0u64, |prefix, seg| {
            *prefix += u64::from(seg.token_estimate.get());
            Some((seg.index, *prefix))
        })
        .filter(|&(index, prefix)| {
            index >= lookback_start
                && prefix >= floor
                && !is_recency_protected(&segments[index], segments, cfg)
        })
        .map(|(index, _)| index)
        .collect();
    BreakpointPlan {
        positions: cap_cache_hints_to(positions, cfg.cache_hint_cap),
    }
}

/// Cap `positions` to [`CACHE_HINT_CAP`], dropping the earliest first.
pub fn cap_cache_hints(positions: Vec<usize>) -> Vec<usize> {
    cap_cache_hints_to(positions, CACHE_HINT_CAP)
}

fn cap_cache_hints_to(mut positions: Vec<usize>, cap: usize) -> Vec<usize> {
    if positions.len() <= cap {
        return positions;
    }
    positions.split_off(positions.len() - cap)
}
