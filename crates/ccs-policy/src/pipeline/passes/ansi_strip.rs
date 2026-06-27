//! Phase 3 pass D — strip ANSI escape sequences, control characters, and cursor/CR
//! progress redraws from a recodeable leaf. Inline-lossless: the model reads the cleaned
//! text, no ref is minted (`ref_id = None`). Idempotent — a second run is a no-op.
//!
//! What it removes: CSI sequences (`\x1b[…<final>`, the SGR colour codes and cursor
//! moves), the two-byte `\x1b` + final-byte escapes, lone carriage returns (the
//! terminal-progress `\r` redraw), and the C0 control characters except the structural
//! `\n` and `\t`. A `\r\n` collapses to `\n` (the CRLF case pass E also normalizes; doing
//! it here keeps D idempotent on its own).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{inline_recode, recode_leaf};

/// Strips ANSI/control sequences from each recodeable leaf, proposing an inline
/// `Recode` where the cleaned text is strictly shorter.
pub struct AnsiStripPass;

impl Pass for AnsiStripPass {
    fn id(&self) -> PassId {
        PassId("ansi_strip")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            if let Some(p) = inline_recode(seg, &leaf, strip_ansi(&leaf.content), self.id()) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// Remove ANSI escape sequences, lone carriage returns, and C0 control bytes (keeping
/// `\n`/`\t`) from `input`. Pure and idempotent.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\r' => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// Consume the rest of an escape sequence after the leading ESC. A CSI sequence
// (`[` + params + a `0x40..=0x7e` final byte) swallows through its final byte; any other
// escape swallows the single following byte.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            while let Some(&c) = chars.peek() {
                chars.next();
                if matches!(c, '\x40'..='\x7e') {
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_colour_codes() {
        let input = "\x1b[31mred\x1b[0m and \x1b[1;32mbold-green\x1b[0m";
        assert_eq!(strip_ansi(input), "red and bold-green");
    }

    #[test]
    fn shrinks_an_ansi_laden_log() {
        let line = "\x1b[2K\rbuilding \x1b[33m[####    ]\x1b[0m 50%\n";
        let cleaned = strip_ansi(&line.repeat(40));
        assert!(
            cleaned.len() < line.repeat(40).len(),
            "stripped log ({}) shrinks vs raw ({})",
            cleaned.len(),
            line.repeat(40).len(),
        );
        assert!(!cleaned.contains('\x1b'));
        assert!(!cleaned.contains('\r'));
    }

    #[test]
    fn drops_carriage_returns_and_control_bytes_keeps_tab_newline() {
        assert_eq!(strip_ansi("a\r\nb\tc\x07d"), "a\nb\tc d".replace(' ', ""));
        assert_eq!(strip_ansi("a\r\nb\tc\x07d"), "a\nb\tcd");
    }

    #[test]
    fn idempotent() {
        let input = "\x1b[31mx\x1b[0m\rprogress\x07\n";
        let once = strip_ansi(input);
        assert_eq!(strip_ansi(&once), once, "a second strip is a no-op");
    }

    #[test]
    fn no_op_on_clean_text() {
        let clean = "plain text\nwith\ttabs and newlines\n";
        assert_eq!(strip_ansi(clean), clean);
    }
}
