//! Deterministic segmentation of a wire body into [`Segment`]s, plus the recency
//! boundary. D-1: server-tool blocks fold into `AssistantTurn` and no unpaired
//! `tool_use` is orphaned. D-4: the recency floor stacks on `is_current`/
//! [`fresh_boundary`]. `byte_offset` is the running sum of preceding blocks' raw
//! byte lengths in provider order (tools → system → messages).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{estimate_chars_proxy, ByteOffset, Generation, MessageId, SegmentKind, TokenCount};

use crate::config::PolicyConfig;
use crate::wire::{ContentBlock, MessageContent, Role, WireBody};

/// The recency window: the most recent N segments are always kept verbatim.
/// Tunable via `PolicyConfig`; this is the default (~a few full turns).
pub const RECENCY_WINDOW_N: usize = 3;

/// A contiguous, classified span of the prompt — the unit of keep/evict decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub index: usize,
    pub kind: SegmentKind,
    pub byte_offset: ByteOffset,
    pub token_estimate: TokenCount,
    pub generation: Generation,
    pub pinned: bool,
    pub is_current: bool,
    pub is_true_human: bool,
    pub source_uuids: Vec<MessageId>,
}

/// Segment a wire body into ordered [`Segment`]s (tools → system → messages).
///
/// A client `tool_use` and the matching user `tool_result` (same `tool_use_id`)
/// collapse into one `ToolPair`. Server-tool blocks and unpaired `tool_use`s never
/// dangle: server blocks fold into their `AssistantTurn`, and an in-flight
/// `tool_use` (no following result) marks its `AssistantTurn` pinned. The last
/// segment is always pinned and current.
pub fn segment_prompt(body: &WireBody) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut offset = 0usize;
    let mut generation = 0u32;

    if let Some(tools) = body.tools {
        push(
            &mut segments,
            &mut offset,
            Build {
                kind: SegmentKind::Tools,
                generation: 0,
                pinned: false,
                is_true_human: false,
                source_uuids: Vec::new(),
                raws: vec![tools],
                rendered: tools.get().to_owned(),
            },
        );
    }
    if let Some(system) = body.system {
        push(
            &mut segments,
            &mut offset,
            Build {
                kind: SegmentKind::System,
                generation: 0,
                pinned: false,
                is_true_human: false,
                source_uuids: Vec::new(),
                raws: vec![system],
                rendered: system.get().to_owned(),
            },
        );
    }

    let msgs = &body.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        match m.role {
            Role::Assistant => match paired_user(msgs, i) {
                Some(j) => {
                    let mut raws = m.content.raws();
                    raws.extend(msgs[j].content.raws());
                    push(
                        &mut segments,
                        &mut offset,
                        Build {
                            kind: SegmentKind::ToolPair,
                            generation,
                            pinned: false,
                            is_true_human: false,
                            source_uuids: vec![message_id(i), message_id(j)],
                            raws,
                            rendered: format!(
                                "{}{}",
                                m.content.rendered(),
                                msgs[j].content.rendered()
                            ),
                        },
                    );
                    i = j + 1;
                }
                None => {
                    push(
                        &mut segments,
                        &mut offset,
                        Build {
                            kind: SegmentKind::AssistantTurn,
                            generation,
                            pinned: has_client_tool_use(&m.content),
                            is_true_human: false,
                            source_uuids: vec![message_id(i)],
                            raws: m.content.raws(),
                            rendered: m.content.rendered(),
                        },
                    );
                    i += 1;
                }
            },
            Role::User => {
                generation += 1;
                push(
                    &mut segments,
                    &mut offset,
                    Build {
                        kind: SegmentKind::UserTurn,
                        generation,
                        pinned: false,
                        is_true_human: is_true_human(&m.content),
                        source_uuids: vec![message_id(i)],
                        raws: m.content.raws(),
                        rendered: m.content.rendered(),
                    },
                );
                i += 1;
            }
            // An injected system-reminder turn (SessionStart context, the
            // deferred-tools notice). Never a true-human turn and never a new
            // conversation round, so it carries the current generation and is a
            // squash candidate like any other message-indexed segment.
            Role::System => {
                push(
                    &mut segments,
                    &mut offset,
                    Build {
                        kind: SegmentKind::System,
                        generation,
                        pinned: false,
                        is_true_human: false,
                        source_uuids: vec![message_id(i)],
                        raws: m.content.raws(),
                        rendered: m.content.rendered(),
                    },
                );
                i += 1;
            }
        }
    }

    if let Some(last) = segments.last_mut() {
        last.pinned = true;
        last.is_current = true;
    }
    segments
}

/// The freshness cutoff: the generation at or above which a segment is recent and
/// always kept verbatim (the second-most-recent user generation, `gen[-2]`). Under
/// budget pressure the caller tightens this to `gen[-1]`; the recency floor stacks
/// on top of it.
pub fn fresh_boundary(segments: &[Segment]) -> Generation {
    let mut gens: Vec<u32> = segments
        .iter()
        .map(|s| s.generation.get())
        .filter(|&g| g > 0)
        .collect();
    gens.sort_unstable();
    gens.dedup();
    Generation(gens.get(gens.len().saturating_sub(2)).copied().unwrap_or(0))
}

/// Whether `seg` sits inside the recency window — the most recent
/// [`PolicyConfig::recency_window_n`] segments, which are never compaction
/// candidates regardless of pressure. A hard, position-based floor that stacks on
/// the structural pins.
pub fn is_recency_protected(seg: &Segment, segments: &[Segment], cfg: &PolicyConfig) -> bool {
    seg.index >= segments.len().saturating_sub(cfg.recency_window_n)
}

/// Whether `seg` may be compacted: it is neither structurally pinned (last segment
/// or in-flight tool_use) nor inside the recency window.
pub fn is_prune_candidate(seg: &Segment, segments: &[Segment], cfg: &PolicyConfig) -> bool {
    !seg.pinned && !is_recency_protected(seg, segments, cfg)
}

struct Build<'a> {
    kind: SegmentKind,
    generation: u32,
    pinned: bool,
    is_true_human: bool,
    source_uuids: Vec<MessageId>,
    raws: Vec<&'a serde_json::value::RawValue>,
    rendered: String,
}

fn push(segments: &mut Vec<Segment>, offset: &mut usize, build: Build) {
    let index = segments.len();
    let byte_offset = ByteOffset(*offset);
    let token_estimate = estimate_chars_proxy(&build.rendered);
    *offset += build.raws.iter().map(|r| r.get().len()).sum::<usize>();
    segments.push(Segment {
        index,
        kind: build.kind,
        byte_offset,
        token_estimate,
        generation: Generation(build.generation),
        pinned: build.pinned,
        is_current: false,
        is_true_human: build.is_true_human,
        source_uuids: build.source_uuids,
    });
}

fn message_id(index: usize) -> MessageId {
    MessageId::new(index.to_string())
}

fn has_client_tool_use(content: &MessageContent) -> bool {
    content
        .blocks()
        .iter()
        .any(ContentBlock::is_client_tool_use)
}

/// The index of the user message that closes the client `tool_use`(s) in the
/// assistant message at `i`, if the immediately following message is a user turn
/// carrying a matching `tool_result`.
fn paired_user(msgs: &[crate::wire::WireMessage], i: usize) -> Option<usize> {
    let use_ids: Vec<&str> = msgs[i]
        .content
        .blocks()
        .iter()
        .filter(|b| b.is_client_tool_use())
        .filter_map(ContentBlock::tool_use_id)
        .collect();
    (!use_ids.is_empty())
        .then_some(i + 1)
        .filter(|&j| j < msgs.len() && msgs[j].role == Role::User)
        .filter(|&j| {
            msgs[j].content.blocks().iter().any(|b| {
                b.is_tool_result() && b.tool_use_id().is_some_and(|id| use_ids.contains(&id))
            })
        })
}

/// Whether a user message is a genuine typed human prompt, on the WIRE.
///
/// The wire request body does NOT carry the transcript's `origin.kind`,
/// `promptSource`, `isMeta`, or `isCompactSummary` fields — those live only in the
/// on-disk `.jsonl` and are Layer 5's job. The sole wire-available discriminator is
/// the *shape* of `content`: a genuine typed prompt is a JSON **string**, whereas
/// tool_results and synthetic injections are arrays of blocks. Interrupts
/// (`"[Request interrupted by user…]"`) are string content, so they are correctly
/// treated as true-human. Layer 5 later refines this with cc-transcript's
/// `spec_keep` over the real transcript record.
fn is_true_human(content: &MessageContent) -> bool {
    content.is_string()
}
