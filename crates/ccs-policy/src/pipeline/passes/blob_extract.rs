//! Phase 3 pass F — extract a large base64/data-URI blob embedded in a recodeable leaf's
//! text, replacing the blob run with a short marker. Ref-backed: the byte-exact original
//! leaf is stored so a `retrieve` returns the blob verbatim (`ref_id` minted off-path).
//!
//! Scope is text inside a `tool_result`/`text` leaf — a base64 payload pasted into command
//! output or a `data:…;base64,…` URI. It is NOT the Anthropic-native image-block path: a
//! real image block is a structured `{"type":"image",…}` block, never a recodeable text
//! leaf, so [`recode_leaf`](super::recode::recode_leaf) never yields one here. The pass
//! replaces only the longest blob run; the surrounding text is preserved so the model keeps
//! the context around the elision.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode};

/// The minimum run length of base64 characters to treat as an extractable blob. Below this
/// a run is plausibly real prose (a token, a hash) and not worth a ref round-trip.
const MIN_BLOB_CHARS: usize = 512;

/// Replaces the longest embedded base64 blob in each recodeable leaf with a marker,
/// proposing a ref-backed `Recode` where the result is strictly shorter.
pub struct BlobExtractPass;

impl Pass for BlobExtractPass {
    fn id(&self) -> PassId {
        PassId("blob_extract")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            let Some(extracted) = extract_blob(&leaf.content) else {
                continue;
            };
            if let Some(p) = ref_recode(
                seg,
                &leaf,
                extracted,
                leaf.content.clone().into_bytes(),
                self.id(),
            ) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

fn is_b64(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_' | '=')
}

/// The longest run of base64 characters in `input` as a `(start, end)` byte range, when it
/// reaches [`MIN_BLOB_CHARS`]. ASCII-only, so byte and char offsets coincide over the run.
fn longest_blob(input: &str) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    let (mut best, mut run_start) = (None::<(usize, usize)>, 0usize);
    let mut in_run = false;
    for (i, &b) in bytes.iter().enumerate() {
        match is_b64(b as char) {
            true if !in_run => {
                in_run = true;
                run_start = i;
            }
            true => {}
            false if in_run => {
                in_run = false;
                best = wider(best, (run_start, i));
            }
            false => {}
        }
    }
    if in_run {
        best = wider(best, (run_start, bytes.len()));
    }
    best.filter(|&(s, e)| e - s >= MIN_BLOB_CHARS)
}

fn wider(best: Option<(usize, usize)>, run: (usize, usize)) -> Option<(usize, usize)> {
    match best {
        Some((s, e)) if e - s >= run.1 - run.0 => Some((s, e)),
        _ => Some(run),
    }
}

/// Replace the longest base64 blob in `input` with a `[base64 blob · N bytes elided]`
/// marker, keeping the surrounding text. `None` when no run reaches [`MIN_BLOB_CHARS`].
pub fn extract_blob(input: &str) -> Option<String> {
    let (start, end) = longest_blob(input)?;
    Some(format!(
        "{}[base64 blob · {} bytes elided]{}",
        &input[..start],
        end - start,
        &input[end..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(n: usize) -> String {
        "QWxhZGRpbjpvcGVuIHNlc2FtrZQ"
            .chars()
            .cycle()
            .take(n)
            .collect()
    }

    #[test]
    fn extracts_large_blob_and_shrinks() {
        let input = format!("prefix text\ndata:image/png;base64,{}\nsuffix", blob(2000));
        let out = extract_blob(&input).expect("blob extracted");
        assert!(out.contains("prefix text"));
        assert!(out.contains("suffix"));
        assert!(out.contains("bytes elided]"));
        assert!(out.len() < input.len(), "extraction shrinks the leaf");
        assert!(!out.contains(&blob(2000)), "the blob run is gone");
    }

    #[test]
    fn preserves_surrounding_text() {
        let input = format!("before {} after", blob(1000));
        let out = extract_blob(&input).expect("blob extracted");
        assert!(out.starts_with("before ["));
        assert!(out.ends_with("] after"));
    }

    #[test]
    fn no_op_below_threshold() {
        assert_eq!(extract_blob(&format!("token {} end", blob(100))), None);
    }

    #[test]
    fn no_op_on_plain_prose() {
        let prose = "a normal log line with words and spaces, nothing base64-shaped.\n".repeat(20);
        assert_eq!(extract_blob(&prose), None, "spaces break any long b64 run");
    }

    #[test]
    fn picks_the_longest_run() {
        let input = format!("{} gap {} gap {}", blob(600), blob(2000), blob(600));
        let out = extract_blob(&input).expect("blob extracted");
        assert!(out.contains("2000 bytes elided]"));
        assert!(out.contains(&blob(600)), "the shorter runs survive");
    }
}
