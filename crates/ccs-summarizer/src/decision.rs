//! The per-segment [`ContentDecision`] strategy agent: the char-count pre-gate that
//! short-circuits without an LLM call, the call + tolerant JSON extraction + serde
//! parse + [`normalize`](ContentDecision::normalize), and the narrow fail-to-`Keep`
//! fallback.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::ChoiceTag;
use ccs_policy::{ContentDecision, Strategy, PRE_GATE_MIN_CHARS};

use crate::client::SummarizerClient;
use crate::prompts::DECISION_SYSTEM;

const DECISION_MAX_TOKENS: u32 = 10_240;

/// Decide how to compact `content`, given its live `salience_tags`.
///
/// Segments under [`PRE_GATE_MIN_CHARS`] chars are kept without an LLM call. Any
/// upstream, timeout, or parse error fails safe to `Keep`.
pub async fn decide(
    client: &SummarizerClient,
    content: &str,
    salience_tags: &[&str],
) -> ContentDecision {
    let len = content.chars().count();
    if len < PRE_GATE_MIN_CHARS {
        return keep();
    }

    match query(client, content, salience_tags, len).await {
        Ok(decision) => decision,
        Err(error) => {
            tracing::warn!(%error, "content decision failed; keeping segment");
            keep()
        }
    }
}

async fn query(
    client: &SummarizerClient,
    content: &str,
    salience_tags: &[&str],
    len: usize,
) -> Result<ContentDecision, DecideError> {
    let text = client
        .call(
            DECISION_SYSTEM,
            &user_message(content, salience_tags),
            DECISION_MAX_TOKENS,
        )
        .await?;
    let decision = serde_json::from_str::<ContentDecision>(extract_json(&text)?)?.normalize();
    Ok(match decision.pre_gate(len) {
        Some(Strategy::Keep) => keep(),
        _ => decision,
    })
}

fn user_message(content: &str, salience_tags: &[&str]) -> String {
    format!(
        "<salience_tags>{}</salience_tags>\n<content_to_analyze>\n{content}\n</content_to_analyze>",
        salience_tags.join(", ")
    )
}

/// Slice the first balanced `{…}` object out of `text`, tolerating leading or
/// trailing prose around the JSON the model returns.
fn extract_json(text: &str) -> Result<&str, DecideError> {
    let start = text.find('{').ok_or(DecideError::NoJson)?;
    let end = text.rfind('}').ok_or(DecideError::NoJson)?;
    match start <= end {
        true => Ok(&text[start..=end]),
        false => Err(DecideError::NoJson),
    }
}

fn keep() -> ContentDecision {
    ContentDecision {
        choice: ChoiceTag::Keep,
        ranges_to_keep: vec![],
        summary_content: None,
    }
}

#[derive(Debug, thiserror::Error)]
enum DecideError {
    #[error(transparent)]
    Call(#[from] crate::client::SummarizerError),
    #[error("no JSON object in response")]
    NoJson,
    #[error("decision JSON did not parse: {0}")]
    Parse(#[from] serde_json::Error),
}
