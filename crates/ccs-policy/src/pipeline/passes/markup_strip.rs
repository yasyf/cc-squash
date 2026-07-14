//! Phase 3 pass I — strip HTML markup from recodeable leaves while preserving text nodes.
//! Script, style, and comment bodies are removed; the byte-exact original remains available
//! through the off-path ref-backed recode.
//!
//! The scanner is quote-aware (a `>` inside a quoted attribute does not close its tag), a
//! `<` not followed by an ASCII letter, `/`, or `!` is literal text (the HTML5 tokenizer
//! rule), closing block-level tags emit line separators so blocks don't glue, and
//! `pre`/`code` text keeps its whitespace untouched (no collapsing, no separator
//! injection) while entities decode everywhere — the output is the rendered text, so
//! `&amp;` in a code sample becomes `&`. Bail-outs return `None` — a wrong strip is
//! worse than no strip.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode};

/// The minimum number of known HTML tag occurrences required to classify a leaf as HTML
/// when it has no doctype or root `html` prefix.
const MIN_TAG_HITS: usize = 8;

/// The minimum number of DISTINCT tag names among those hits — repeated single-tag string
/// literals in source code must not qualify.
const MIN_DISTINCT_TAGS: usize = 3;

/// The maximum run of UNQUOTED bytes scanned inside a tag before it is declared malformed.
/// Quoted attribute values (e.g. a base64 data URI) are exempt from the window.
const UNQUOTED_TAG_WINDOW: usize = 256;

const HTML_TAGS: &[&[u8]] = &[
    b"div", b"p", b"a", b"span", b"li", b"ul", b"ol", b"table", b"tr", b"td", b"br", b"head",
    b"body", b"title", b"h1", b"h2", b"h3", b"h4", b"h5", b"h6", b"script", b"style",
];

const BLOCK_TAGS: &[&[u8]] = &[
    b"p",
    b"div",
    b"li",
    b"tr",
    b"table",
    b"ul",
    b"ol",
    b"h1",
    b"h2",
    b"h3",
    b"h4",
    b"h5",
    b"h6",
    b"br",
    b"section",
    b"article",
    b"header",
    b"footer",
    b"pre",
    b"blockquote",
];

const CELL_TAGS: &[&[u8]] = &[b"td", b"th"];

/// Removes HTML markup from each recodeable leaf, proposing a ref-backed `Recode` where
/// the result is strictly shorter.
pub struct MarkupStripPass;

impl Pass for MarkupStripPass {
    fn id(&self) -> PassId {
        PassId("markup_strip")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            if !looks_like_html(&leaf.content) {
                continue;
            }
            let Some(rendered) = strip_html(&leaf.content) else {
                continue;
            };
            if let Some(p) = ref_recode(
                seg,
                &leaf,
                rendered,
                leaf.content.clone().into_bytes(),
                self.id(),
            ) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// Whether `input` is conservatively recognizable as HTML: a doctype/root `html` prefix, or
/// a leaf that itself starts with a tag and clears both the hit and distinct-tag floors. An
/// XML declaration always wins over the HTML prefix and tag-density checks.
pub fn looks_like_html(input: &str) -> bool {
    let trimmed = input.trim();
    if starts_ascii_case_insensitive(trimmed, b"<?xml") {
        return false;
    }
    if starts_ascii_case_insensitive(trimmed, b"<!doctype html")
        || starts_ascii_case_insensitive(trimmed, b"<html")
    {
        return true;
    }
    if !trimmed.starts_with('<') {
        return false;
    }

    let bytes = trimmed.as_bytes();
    let mut hits = 0;
    let mut seen: u32 = 0;
    for index in 0..bytes.len() {
        if bytes[index] != b'<' {
            continue;
        }
        if let Some(tag) = HTML_TAGS
            .iter()
            .position(|tag| named_tag_at(bytes, index, tag))
        {
            hits += 1;
            seen |= 1 << tag;
            if hits >= MIN_TAG_HITS && seen.count_ones() as usize >= MIN_DISTINCT_TAGS {
                return true;
            }
        }
    }
    false
}

/// Strip HTML tags, comments, script/style spans, and common entities from `input`.
/// Closing block-level tags become newlines (a space for table cells); `pre`/`code`
/// whitespace stays untouched while entities decode everywhere (the output is rendered
/// text); blank-line runs elsewhere collapse to at most one blank line. Returns `None`
/// for malformed markup or when the rendered text does not strictly shrink.
pub fn strip_html(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut renderer = Renderer::new(input.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'<' if bytes[index..].starts_with(b"<!--") => {
                index = find_bytes(bytes, index + 4, b"-->")? + 3;
            }
            b'<' if starts_tag(*bytes.get(index + 1)?) => {
                let tag_end = find_tag_end(bytes, index)?;
                if named_opening_tag_at(bytes, index, b"script") {
                    let close = find_closing_tag(bytes, tag_end + 1, b"script")?;
                    index = find_tag_end(bytes, close)? + 1;
                } else if named_opening_tag_at(bytes, index, b"style") {
                    let close = find_closing_tag(bytes, tag_end + 1, b"style")?;
                    index = find_tag_end(bytes, close)? + 1;
                } else {
                    renderer.tag(&Tag::parse(bytes, index, tag_end));
                    index = tag_end + 1;
                }
            }
            b'<' => {
                renderer.push('<');
                index += 1;
            }
            b'&' => {
                if let Some((decoded, width)) = decoded_entity(&bytes[index..]) {
                    renderer.push(decoded);
                    index += width;
                } else {
                    renderer.push('&');
                    index += 1;
                }
            }
            _ => {
                let ch = input[index..].chars().next()?;
                renderer.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    let rendered = renderer.finish();
    (rendered.len() < input.len()).then_some(rendered)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Separator {
    Space,
    Newline,
}

struct Tag<'a> {
    name: &'a [u8],
    closing: bool,
    self_closing: bool,
}

impl<'a> Tag<'a> {
    fn parse(bytes: &'a [u8], start: usize, end: usize) -> Self {
        let closing = bytes.get(start + 1) == Some(&b'/');
        let name_start = start + 1 + usize::from(closing);
        let name_end = name_start
            + bytes[name_start..end]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric())
                .count();
        Tag {
            name: &bytes[name_start..name_end],
            closing,
            self_closing: bytes[end - 1] == b'/',
        }
    }

    fn is(&self, name: &[u8]) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    fn separator(&self) -> Option<Separator> {
        if self.is(b"br") {
            return Some(Separator::Newline);
        }
        if !self.closing && !self.self_closing {
            return None;
        }
        if CELL_TAGS.iter().any(|name| self.is(name)) {
            return Some(Separator::Space);
        }
        BLOCK_TAGS
            .iter()
            .any(|name| self.is(name))
            .then_some(Separator::Newline)
    }
}

/// Text accumulator: injects a single pending separator per block boundary (consecutive
/// closing tags collapse into one), caps newline runs at two outside `pre`/`code`, and
/// preserves bytes verbatim inside. A pending separator at end of input is dropped.
struct Renderer {
    out: String,
    trailing_newlines: usize,
    pending: Option<Separator>,
    pre_depth: usize,
}

impl Renderer {
    fn new(capacity: usize) -> Self {
        Renderer {
            out: String::with_capacity(capacity),
            trailing_newlines: 0,
            pending: None,
            pre_depth: 0,
        }
    }

    fn tag(&mut self, tag: &Tag) {
        if tag.is(b"pre") || tag.is(b"code") {
            match (tag.closing, tag.self_closing) {
                (true, _) => self.pre_depth = self.pre_depth.saturating_sub(1),
                (false, false) => self.pre_depth += 1,
                (false, true) => {}
            }
        }
        // A closing `pre` schedules its block separator here, AFTER the depth drop above, so
        // the separator fires outside the protected span the model reads byte-for-byte.
        if let Some(sep) = tag.separator() {
            if self.pre_depth == 0 && !self.out.is_empty() {
                self.pending = Some(self.pending.take().map_or(sep, |prior| prior.max(sep)));
            }
        }
    }

    fn push(&mut self, ch: char) {
        if self.pre_depth > 0 {
            self.flush_pending();
            self.raw(ch);
            return;
        }
        match (self.pending, ch) {
            (Some(Separator::Newline), '\n') => self.pending = None,
            (Some(Separator::Newline), ' ' | '\t') => return,
            (Some(Separator::Space), c) if c.is_whitespace() => self.pending = None,
            (Some(_), _) => self.flush_pending(),
            (None, _) => {}
        }
        match ch {
            '\n' => self.newline_capped(),
            _ => self.raw(ch),
        }
    }

    fn flush_pending(&mut self) {
        match self.pending.take() {
            Some(Separator::Newline) => self.newline_capped(),
            Some(Separator::Space) => self.raw(' '),
            None => {}
        }
    }

    fn newline_capped(&mut self) {
        if self.trailing_newlines < 2 {
            self.raw('\n');
        }
    }

    fn raw(&mut self, ch: char) {
        self.trailing_newlines = match ch {
            '\n' => self.trailing_newlines + 1,
            _ => 0,
        };
        self.out.push(ch);
    }

    fn finish(self) -> String {
        self.out
    }
}

fn starts_tag(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'/' | b'!')
}

fn starts_ascii_case_insensitive(input: &str, prefix: &[u8]) -> bool {
    matches!(
        input.as_bytes().get(..prefix.len()),
        Some(candidate) if candidate.eq_ignore_ascii_case(prefix)
    )
}

fn named_tag_at(bytes: &[u8], start: usize, name: &[u8]) -> bool {
    named_opening_tag_at(bytes, start, name) || named_closing_tag_at(bytes, start, name)
}

fn named_opening_tag_at(bytes: &[u8], start: usize, name: &[u8]) -> bool {
    named_tag_with_prefix_at(bytes, start, name, b"<")
}

fn named_closing_tag_at(bytes: &[u8], start: usize, name: &[u8]) -> bool {
    named_tag_with_prefix_at(bytes, start, name, b"</")
}

fn named_tag_with_prefix_at(bytes: &[u8], start: usize, name: &[u8], prefix: &[u8]) -> bool {
    let name_start = start + prefix.len();
    let name_end = name_start + name.len();
    matches!(bytes.get(start..name_start), Some(candidate) if candidate == prefix)
        && matches!(bytes.get(name_start..name_end), Some(candidate) if candidate.eq_ignore_ascii_case(name))
        && match bytes.get(name_end) {
            Some(byte) => is_tag_boundary(*byte),
            None => true,
        }
}

/// A tag name terminates only at ASCII whitespace, `/`, or `>` (the HTML5 tokenizer rule).
/// Any other trailing byte — `!` in `</script!>`, a letter in `</scriptx>` — continues the
/// name, so the candidate is NOT the tag being matched.
fn is_tag_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
}

/// The index of the `>` closing the tag opened at `start`. Quoted attribute spans are
/// skipped whole — a `>` inside quotes does not close the tag, and quoted content is
/// exempt from the malformed-input window. `None` on an unterminated quote, on a run of
/// unquoted tag content past [`UNQUOTED_TAG_WINDOW`], or when the input ends mid-tag.
fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    let mut unquoted_run = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'>' => return Some(index),
            b'"' | b'\'' => {
                index = find_byte(bytes, index + 1, byte)? + 1;
                unquoted_run = 0;
            }
            _ => {
                unquoted_run += 1;
                if unquoted_run > UNQUOTED_TAG_WINDOW {
                    return None;
                }
                index += 1;
            }
        }
    }
    None
}

fn find_byte(bytes: &[u8], start: usize, needle: u8) -> Option<usize> {
    bytes
        .get(start..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| start + offset)
}

fn find_bytes(bytes: &[u8], start: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

fn find_closing_tag(bytes: &[u8], start: usize, name: &[u8]) -> Option<usize> {
    let mut cursor = start;
    while cursor < bytes.len() {
        let offset = bytes.get(cursor..)?.iter().position(|byte| *byte == b'<')?;
        let candidate = cursor + offset;
        if named_closing_tag_at(bytes, candidate, name) {
            return Some(candidate);
        }
        cursor = candidate + 1;
    }
    None
}

fn decoded_entity(bytes: &[u8]) -> Option<(char, usize)> {
    [
        (b"&amp;".as_slice(), '&'),
        (b"&lt;".as_slice(), '<'),
        (b"&gt;".as_slice(), '>'),
        (b"&quot;".as_slice(), '"'),
        (b"&#39;".as_slice(), '\''),
        (b"&nbsp;".as_slice(), ' '),
    ]
    .into_iter()
    .find_map(|(entity, decoded)| bytes.starts_with(entity).then_some((decoded, entity.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_html_prefixes_case_insensitively() {
        assert!(looks_like_html("  <!DoCtYpE HTML><title>page</title>"));
        assert!(looks_like_html(
            "\n<HTML lang=\"en\"><body>page</body></HTML>"
        ));
    }

    #[test]
    fn recognizes_only_at_the_tag_hit_threshold() {
        assert!(
            !looks_like_html(&format!("<main>{}</main>", "<div>text</div>".repeat(4))),
            "8 hits from a single tag name stay below the distinct floor"
        );
        assert!(
            !looks_like_html("<div><ul><li>a</li></ul></div>"),
            "3 distinct names but only 6 hits stay below the hit floor"
        );
        assert!(looks_like_html("<DiV><ul><LI>a</li><li>b</LI></UL></div>"));
    }

    #[test]
    fn does_not_treat_generics_as_html() {
        let source = "let values: Vec<String> = Vec::new();\n\
                      let map: HashMap<K, V> = HashMap::new();\n\
                      if left < right && right > floor { work(); }\n";
        assert!(!looks_like_html(&source.repeat(20)));
    }

    #[test]
    fn xml_declaration_excludes_tag_dense_documents() {
        let xml = format!(
            "<?xml version=\"1.0\"?><root>{}</root>",
            "<div><p>x</p></div>".repeat(8)
        );
        assert!(!looks_like_html(&xml));
    }

    #[test]
    fn decodes_entities_and_uses_ascii_space_for_nbsp() {
        let output = strip_html("<p>A &amp; B &lt; C &gt; D &quot;q&quot; &#39;s&#39;&nbsp;x</p>")
            .expect("HTML shrinks");
        assert_eq!(output, "A & B < C > D \"q\" 's' x");
    }

    #[test]
    fn preserves_pre_and_code_text() {
        let output =
            strip_html("<pre><code>let answer = 42;\nprintln!(\"{answer}\");</code></pre>")
                .expect("HTML shrinks");
        assert_eq!(output, "let answer = 42;\nprintln!(\"{answer}\");");
    }

    #[test]
    fn decodes_entities_inside_pre_as_rendered_text() {
        assert_eq!(
            strip_html("<pre>if (a &lt; b &amp;&amp; c) { run(); }</pre>"),
            Some("if (a < b && c) { run(); }".to_owned())
        );
    }

    #[test]
    fn removes_script_style_and_comments_non_greedily() {
        let input = "<ScRiPt>first()</sCrIpT><p>kept</p><STYLE>.gone{}</style><!-- gone -->";
        assert_eq!(strip_html(input), Some("kept".to_owned()));
    }

    #[test]
    fn markup_strip_ignores_invalid_script_close() {
        // `</script!>` is not a valid end tag, so the span runs to the real `</script>`.
        assert_eq!(
            strip_html("<script>one</script!>two</script><p>kept</p>"),
            Some("kept".to_owned())
        );
    }

    #[test]
    fn markup_strip_separates_pre_from_following_block() {
        assert_eq!(strip_html("<pre>a</pre><p>b</p>"), Some("a\nb".to_owned()));
        assert_eq!(
            strip_html("<blockquote>a</blockquote><p>b</p>"),
            Some("a\nb".to_owned())
        );
    }

    #[test]
    fn preserves_multibyte_text_at_tag_boundaries() {
        assert_eq!(
            strip_html("<p>café 猫 &amp; чай</p>"),
            Some("café 猫 & чай".to_owned()),
        );
    }

    #[test]
    fn malformed_markup_aborts() {
        assert_eq!(strip_html("<div>kept</div><unfinished"), None);
        assert_eq!(strip_html("<script>never closed"), None);
        assert_eq!(strip_html("<style>never closed"), None);
        assert_eq!(strip_html("<!-- never closed"), None);
    }

    #[test]
    fn collapses_excess_blank_lines() {
        assert_eq!(
            strip_html("<div>a</div>\n\n\n\n<div>b</div>"),
            Some("a\n\nb".to_owned())
        );
    }

    #[test]
    fn unchanged_output_does_not_pass_size_gate() {
        assert_eq!(strip_html("plain text without markup"), None);
    }

    #[test]
    fn markup_strip_keeps_text_with_gt_inside_quoted_attr() {
        assert_eq!(
            strip_html("<p title=\"1 > 0\">alpha</p>"),
            Some("alpha".to_owned())
        );
    }

    #[test]
    fn markup_strip_keeps_bare_lt_as_text() {
        assert_eq!(strip_html("<p>2 < 3</p>"), Some("2 < 3".to_owned()));
    }

    #[test]
    fn markup_strip_no_op_on_source_with_repeated_html_literals() {
        let source = "let x = \"<div>a</div>\";\n".repeat(20);
        assert!(!looks_like_html(&source));
    }

    #[test]
    fn markup_strip_separates_block_elements() {
        assert_eq!(
            strip_html("<p>alpha</p><p>beta</p>"),
            Some("alpha\nbeta".to_owned())
        );
    }

    #[test]
    fn markup_strip_preserves_pre_blank_lines() {
        assert_eq!(
            strip_html("<p>intro</p><pre>line1\n\n\n\nline2</pre>"),
            Some("intro\nline1\n\n\n\nline2".to_owned()),
        );
    }

    #[test]
    fn markup_strip_strips_page_with_long_data_uri_attr() {
        let page = format!(
            "<p>before</p><img src=\"data:image/png;base64,{}\"/><p>after</p>",
            "A".repeat(10_000),
        );
        assert_eq!(strip_html(&page), Some("before\nafter".to_owned()));
    }

    #[test]
    fn markup_strip_bails_on_unterminated_quote() {
        assert_eq!(strip_html("<p class=\"never closed>text"), None);
    }

    #[test]
    fn separates_table_cells_with_spaces() {
        assert_eq!(
            strip_html("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>"),
            Some("a b\nc".to_owned()),
        );
    }
}
