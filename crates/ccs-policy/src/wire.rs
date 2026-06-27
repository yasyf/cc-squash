//! The serde parse boundary. [`WireBody`]/[`WireMessage`]/[`ContentBlock`] borrow
//! from the buffered request bytes via `&RawValue`, keeping untouched subtrees
//! byte-exact (thinking signatures survive). Closed variants for the shapes we
//! recognize; an open [`ContentBlock::Other`] arm folds any unknown wire shape
//! conservatively (treated as non-rewritable).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;

use ccs_core::ModelId;
use serde::Deserialize;
use serde_json::value::RawValue;

/// A message author role. Beyond `user`/`assistant`, Claude Code injects
/// `system`-role messages into the `messages[]` array (SessionStart hook context,
/// system-reminders, the deferred-tools notice) — distinct from the top-level
/// `system` prompt field. Rejecting that variant fails the whole body parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// A parsed Anthropic Messages request body. Owned scalars (`model`, `max_tokens`)
/// are copied out; every structural subtree (`system`, `tools`, message content)
/// stays a borrowed `&RawValue` so the verbatim prefix is reproducible byte-exact.
#[derive(Debug, Deserialize)]
pub struct WireBody<'a> {
    pub model: ModelId,
    #[serde(borrow, default)]
    pub system: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub tools: Option<&'a RawValue>,
    #[serde(borrow)]
    pub messages: Vec<WireMessage<'a>>,
    pub max_tokens: u32,
}

/// One `messages[]` entry: a role and its content, which the wire carries either as
/// a JSON string (a genuine typed prompt) or as an array of blocks.
#[derive(Debug, Deserialize)]
pub struct WireMessage<'a> {
    pub role: Role,
    #[serde(borrow)]
    pub content: MessageContent<'a>,
}

/// A message's `content`: either a raw JSON string (the wire's true-human
/// discriminator) or an ordered array of [`ContentBlock`]s. Both forms keep their
/// verbatim `&RawValue` spans for byte-exact accounting.
#[derive(Debug, Clone)]
pub enum MessageContent<'a> {
    /// String content — kept both decoded (`text`) and verbatim (`raw`).
    Text {
        raw: &'a RawValue,
        text: Cow<'a, str>,
    },
    /// An array of content blocks.
    Blocks(Vec<ContentBlock<'a>>),
}

/// A single content block, tagged by its `type`. Each arm carries the block's full
/// verbatim bytes as a `&RawValue`; the closed arms are the shapes the engine
/// reasons about, and [`ContentBlock::Other`] captures anything else unchanged.
#[derive(Debug, Clone)]
pub enum ContentBlock<'a> {
    Text(&'a RawValue),
    ToolUse(&'a RawValue),
    ToolResult(&'a RawValue),
    Thinking(&'a RawValue),
    RedactedThinking(&'a RawValue),
    ServerToolUse(&'a RawValue),
    ServerToolResult(&'a RawValue),
    Other(&'a RawValue),
}

/// Parse a buffered request body into a borrowed [`WireBody`]. Untouched subtrees
/// stay byte-exact because every structural field is a `&RawValue` into `bytes`.
pub fn parse_body(bytes: &[u8]) -> Result<WireBody<'_>, serde_json::Error> {
    serde_json::from_slice(bytes)
}

impl<'a> MessageContent<'a> {
    /// Whether the wire content is a JSON string — the true-human discriminator.
    pub fn is_string(&self) -> bool {
        matches!(self, Self::Text { .. })
    }

    /// The content blocks, or an empty slice when the content is a string.
    pub fn blocks(&self) -> &[ContentBlock<'a>] {
        match self {
            Self::Blocks(blocks) => blocks,
            Self::Text { .. } => &[],
        }
    }

    /// The verbatim `&RawValue` spans backing this content — one per block, or the
    /// single string value. Their byte lengths drive `byte_offset` accounting.
    pub fn raws(&self) -> Vec<&'a RawValue> {
        match self {
            Self::Text { raw, .. } => vec![raw],
            Self::Blocks(blocks) => blocks.iter().map(ContentBlock::raw).collect(),
        }
    }

    /// The text used for token estimation: the decoded string, or the concatenated
    /// verbatim bytes of the blocks.
    pub fn rendered(&self) -> String {
        match self {
            Self::Text { text, .. } => text.as_ref().to_owned(),
            Self::Blocks(blocks) => blocks.iter().map(|b| b.raw().get()).collect(),
        }
    }
}

impl<'a> ContentBlock<'a> {
    /// The block's verbatim bytes.
    pub fn raw(&self) -> &'a RawValue {
        match *self {
            Self::Text(raw)
            | Self::ToolUse(raw)
            | Self::ToolResult(raw)
            | Self::Thinking(raw)
            | Self::RedactedThinking(raw)
            | Self::ServerToolUse(raw)
            | Self::ServerToolResult(raw)
            | Self::Other(raw) => raw,
        }
    }

    /// Whether this is a client `tool_use` block (the only kind that pairs with a
    /// following user `tool_result`). Server tools never pair.
    pub fn is_client_tool_use(&self) -> bool {
        matches!(self, Self::ToolUse(_))
    }

    /// Whether this is a client `tool_result` block.
    pub fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult(_))
    }

    /// The id that pairs a `tool_use` with its `tool_result`: the `id` field of a
    /// `tool_use`/`server_tool_use`, or the `tool_use_id` field of a
    /// `tool_result`/server result. `None` for any other block.
    pub fn tool_use_id(&self) -> Option<&'a str> {
        let fields: IdFields<'a> = serde_json::from_str(self.raw().get()).ok()?;
        match self {
            Self::ToolUse(_) | Self::ServerToolUse(_) => fields.id,
            Self::ToolResult(_) | Self::ServerToolResult(_) => fields.tool_use_id,
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct IdFields<'a> {
    #[serde(borrow, default)]
    id: Option<&'a str>,
    #[serde(borrow, default)]
    tool_use_id: Option<&'a str>,
}

fn classify(raw: &RawValue) -> ContentBlock<'_> {
    #[derive(Deserialize)]
    struct TypeTag {
        #[serde(default)]
        r#type: Option<String>,
    }

    match serde_json::from_str::<TypeTag>(raw.get())
        .ok()
        .and_then(|t| t.r#type)
        .as_deref()
    {
        Some("text") => ContentBlock::Text(raw),
        Some("tool_use") => ContentBlock::ToolUse(raw),
        Some("tool_result") => ContentBlock::ToolResult(raw),
        Some("thinking") => ContentBlock::Thinking(raw),
        Some("redacted_thinking") => ContentBlock::RedactedThinking(raw),
        Some("server_tool_use") => ContentBlock::ServerToolUse(raw),
        Some("web_search_tool_result")
        | Some("web_fetch_tool_result")
        | Some("code_execution_tool_result") => ContentBlock::ServerToolResult(raw),
        _ => ContentBlock::Other(raw),
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for MessageContent<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let raw: &'a RawValue = Deserialize::deserialize(deserializer)?;
        match raw.get().trim_start().starts_with('"') {
            true => Ok(MessageContent::Text {
                raw,
                text: serde_json::from_str(raw.get()).map_err(D::Error::custom)?,
            }),
            false => Ok(MessageContent::Blocks(
                serde_json::from_str(raw.get()).map_err(D::Error::custom)?,
            )),
        }
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for ContentBlock<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(classify(Deserialize::deserialize(deserializer)?))
    }
}
