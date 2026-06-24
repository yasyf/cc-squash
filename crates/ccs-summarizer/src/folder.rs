//! The Rsum recursive [`WorkingState`] folder (§3c): fold the previous state and
//! the new turns into a new state over the pinned model, constraints copied
//! verbatim by the prompt, with a fail-safe back to the prior state.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_policy::WorkingState;

use crate::client::SummarizerClient;
use crate::prompts::WORKING_STATE_SYSTEM;

const FOLD_MAX_TOKENS: u32 = 16_384;

/// Fold `prev` and `new_turns` into the next [`WorkingState`].
///
/// Constraints are copied verbatim by the model per the prompt; this function only
/// deserializes the result. Any upstream, timeout, or parse error returns `prev`
/// unchanged.
pub async fn fold(client: &SummarizerClient, prev: &WorkingState, new_turns: &str) -> WorkingState {
    match query(client, prev, new_turns).await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, "working-state fold failed; keeping prior state");
            prev.clone()
        }
    }
}

async fn query(
    client: &SummarizerClient,
    prev: &WorkingState,
    new_turns: &str,
) -> Result<WorkingState, FoldError> {
    let text = client
        .call(
            WORKING_STATE_SYSTEM,
            &user_message(prev, new_turns),
            FOLD_MAX_TOKENS,
        )
        .await?;
    Ok(serde_json::from_str::<WorkingState>(extract_json(&text)?)?)
}

fn user_message(prev: &WorkingState, new_turns: &str) -> String {
    format!(
        "<previous_working_state>\n{}\n</previous_working_state>\n<new_turns>\n{new_turns}\n</new_turns>",
        serde_json::to_string_pretty(prev).unwrap_or_default()
    )
}

fn extract_json(text: &str) -> Result<&str, FoldError> {
    let start = text.find('{').ok_or(FoldError::NoJson)?;
    let end = text.rfind('}').ok_or(FoldError::NoJson)?;
    match start <= end {
        true => Ok(&text[start..=end]),
        false => Err(FoldError::NoJson),
    }
}

#[derive(Debug, thiserror::Error)]
enum FoldError {
    #[error(transparent)]
    Call(#[from] crate::client::SummarizerError),
    #[error("no JSON object in response")]
    NoJson,
    #[error("working-state JSON did not parse: {0}")]
    Parse(#[from] serde_json::Error),
}
