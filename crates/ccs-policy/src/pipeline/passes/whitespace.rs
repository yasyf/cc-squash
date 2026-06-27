//! Phase 3 pass E — normalize insignificant whitespace in a recodeable leaf. Inline-
//! lossless: the model reads the cleaned text, no ref is minted (`ref_id = None`).
//! Idempotent — a second run is a no-op.
//!
//! Three normalizations: CRLF → LF, strip trailing whitespace from every line, and
//! collapse a run of 3+ blank lines down to a single blank line. The result preserves
//! the line structure a model relies on while shedding the redraw/padding noise.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{inline_recode, recode_leaf};

/// Normalizes whitespace in each recodeable leaf, proposing an inline `Recode` where the
/// result is strictly shorter.
pub struct WhitespacePass;

impl Pass for WhitespacePass {
    fn id(&self) -> PassId {
        PassId("whitespace")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            if let Some(p) = inline_recode(seg, &leaf, normalize_ws(&leaf.content), self.id()) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// CRLF → LF, strip trailing whitespace per line, collapse 3+ blank lines to one. Pure
/// and idempotent. A trailing newline is preserved iff the input had one.
pub fn normalize_ws(input: &str) -> String {
    let trailing_newline = input.ends_with('\n');
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0u32;
    for line in input.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim_end();
        blank_run = if trimmed.is_empty() { blank_run + 1 } else { 0 };
        if blank_run > 1 {
            continue;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    match trailing_newline {
        true => out,
        false => {
            out.truncate(out.trim_end_matches('\n').len());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_blank_runs_and_strips_trailing_ws() {
        let input = "a   \n\n\n\n\nb\t\n";
        assert_eq!(normalize_ws(input), "a\n\nb\n");
    }

    #[test]
    fn crlf_becomes_lf() {
        assert_eq!(normalize_ws("a\r\nb\r\n"), "a\nb\n");
        assert!(!normalize_ws("a\r\nb").contains('\r'));
    }

    #[test]
    fn shrinks_a_padded_blob() {
        let input = format!("header   \n{}done\n", "\n".repeat(50));
        let out = normalize_ws(&input);
        assert!(
            out.len() < input.len(),
            "normalized ({}) shrinks vs raw ({})",
            out.len(),
            input.len(),
        );
    }

    #[test]
    fn idempotent() {
        let input = "x  \n\n\n\n\ny\r\n\r\n\r\n\r\nz   \n";
        let once = normalize_ws(input);
        assert_eq!(normalize_ws(&once), once, "a second normalize is a no-op");
    }

    #[test]
    fn no_op_on_already_clean() {
        let clean = "a\nb\n\nc\n";
        assert_eq!(normalize_ws(clean), clean);
    }

    #[test]
    fn preserves_missing_trailing_newline() {
        assert_eq!(normalize_ws("a  \nb  "), "a\nb");
    }
}
