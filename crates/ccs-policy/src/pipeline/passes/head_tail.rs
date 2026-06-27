//! Phase 3 pass J — deterministic head/tail truncation of a very long plaintext leaf. Keep
//! the first [`HEAD_LINES`] and last [`TAIL_LINES`] lines, elide the middle with a marker.
//! Ref-backed: the byte-exact original is stored so a `retrieve` returns the full log
//! verbatim (`ref_id` minted off-path).
//!
//! Unlike the LLM `Truncate` strategy, the kept ranges here are FIXED (head N, tail M), so
//! the transform is pure and reproducible. The kept line spans are expressed as
//! [`LineRange`]s ([`kept_ranges`]) for provenance/parity with the ladder's `Truncate`
//! machinery; the rendered body is the head + a `[… K lines elided …]` marker + the tail.
//! Fires only when a leaf has more than `HEAD_LINES + TAIL_LINES` lines, so a short leaf is
//! never touched.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::LineRange;

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode};

/// Lines kept from the head of an over-long leaf.
const HEAD_LINES: usize = 40;
/// Lines kept from the tail of an over-long leaf.
const TAIL_LINES: usize = 20;

/// Head/tail-truncates each recodeable leaf with more than `HEAD_LINES + TAIL_LINES` lines,
/// proposing a ref-backed `Recode` where the result is strictly shorter.
pub struct HeadTailPass;

impl Pass for HeadTailPass {
    fn id(&self) -> PassId {
        PassId("head_tail")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            let Some(truncated) = head_tail(&leaf.content) else {
                continue;
            };
            if let Some(p) = ref_recode(
                seg,
                &leaf,
                truncated,
                leaf.content.clone().into_bytes(),
                self.id(),
            ) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// The kept line spans (1-based, inclusive) for a `total`-line leaf: the head and tail
/// blocks. `None` when `total` is short enough to keep whole.
pub fn kept_ranges(total: usize) -> Option<Vec<LineRange>> {
    (total > HEAD_LINES + TAIL_LINES).then(|| {
        vec![
            LineRange {
                start: 1,
                end: HEAD_LINES,
            },
            LineRange {
                start: total - TAIL_LINES + 1,
                end: total,
            },
        ]
    })
}

/// Keep the first [`HEAD_LINES`] and last [`TAIL_LINES`] lines of `input`, eliding the
/// middle with a `[… K lines elided …]` marker. `None` when `input` is short enough to
/// keep whole (at most `HEAD_LINES + TAIL_LINES` lines).
pub fn head_tail(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.lines().collect();
    let total = lines.len();
    if total <= HEAD_LINES + TAIL_LINES {
        return None;
    }
    let elided = total - HEAD_LINES - TAIL_LINES;
    Some(format!(
        "{}\n[… {elided} lines elided …]\n{}",
        lines[..HEAD_LINES].join("\n"),
        lines[total - TAIL_LINES..].join("\n"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(n: usize) -> String {
        (0..n)
            .map(|i| format!("log line number {i} with some payload text"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn truncates_a_long_log_and_shrinks() {
        let input = log(5000);
        let out = head_tail(&input).expect("truncated");
        assert!(out.len() < input.len(), "head/tail shrinks a 5000-line log");
        assert!(out.starts_with("log line number 0 "));
        assert!(out
            .trim_end()
            .ends_with("log line number 4999 with some payload text"));
        assert!(out.contains("lines elided …]"));
        assert_eq!(out.lines().count(), HEAD_LINES + TAIL_LINES + 1);
    }

    #[test]
    fn elided_count_is_exact() {
        let out = head_tail(&log(100)).expect("truncated");
        assert!(out.contains(&format!(
            "[… {} lines elided …]",
            100 - HEAD_LINES - TAIL_LINES
        )));
    }

    #[test]
    fn no_op_on_short_leaf() {
        let short = log(HEAD_LINES + TAIL_LINES);
        assert_eq!(
            head_tail(&short),
            None,
            "exactly at the boundary keeps whole"
        );
        assert_eq!(head_tail(&log(5)), None);
    }

    #[test]
    fn kept_ranges_cover_head_and_tail() {
        assert_eq!(kept_ranges(HEAD_LINES + TAIL_LINES), None);
        let ranges = kept_ranges(5000).expect("ranges");
        assert_eq!(
            ranges,
            vec![
                LineRange {
                    start: 1,
                    end: HEAD_LINES
                },
                LineRange {
                    start: 5000 - TAIL_LINES + 1,
                    end: 5000
                },
            ]
        );
    }
}
