//! The Anthropic `/v1/messages` boundary: the pinned [`SUMMARIZER_MODEL`], the
//! captured [`SessionAuthContext`] reused verbatim so the summarizer's own call
//! inherits the live session's first-party status, and [`SummarizerClient`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde_json::json;

/// The summarizer's pinned model — always latest-Sonnet, a single bump-able
/// constant. The off-path summarizer ignores the live session's model so its cost
/// and latency stay predictable.
pub const SUMMARIZER_MODEL: &str = "claude-sonnet-4-6";

const MESSAGES_PATH: &str = "v1/messages";
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// An error from a [`SummarizerClient::call`].
#[derive(Debug, thiserror::Error)]
pub enum SummarizerError {
    /// The `/v1/messages` POST failed to send or returned a transport error.
    #[error("upstream request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The upstream returned a non-2xx status.
    #[error("upstream returned status {0}")]
    Status(reqwest::StatusCode),
    /// The response body was not the expected Anthropic `messages` shape.
    #[error("malformed upstream response: {0}")]
    Decode(#[from] serde_json::Error),
    /// The call exceeded [`CALL_TIMEOUT`].
    #[error("upstream call timed out")]
    Timeout,
}

/// The live session's captured auth context, reused verbatim by the summarizer.
///
/// The summarizer injects **no** key of its own: it replays the session's captured
/// auth, `anthropic-version`, and `anthropic-beta` headers so its own
/// `/v1/messages` POST inherits the session's first-party status — the way Claude
/// Code's native compaction summarizer works. Layer 3 tests fabricate this;
/// Layer 4 populates it from the intercepted request.
#[derive(Debug, Clone)]
pub struct SessionAuthContext {
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub upstream: Url,
}

/// Calls Anthropic's `/v1/messages` for the off-path summarizer.
///
/// Built over the workspace `reqwest::Client` (no `json` feature): bodies are
/// serialized with `serde_json::to_vec` and responses parsed from `.bytes()`.
#[derive(Debug, Clone)]
pub struct SummarizerClient {
    client: Client,
    ctx: SessionAuthContext,
}

impl SummarizerClient {
    /// Build a client that POSTs to `ctx.upstream` replaying `ctx.headers`.
    pub fn new(ctx: SessionAuthContext) -> Self {
        Self {
            client: Client::new(),
            ctx,
        }
    }

    /// POST a single-turn `{system, user}` request and return the concatenated
    /// text blocks of the response, capped at `max_tokens`.
    pub async fn call(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, SummarizerError> {
        let body = serde_json::to_vec(&json!({
            "model": SUMMARIZER_MODEL,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        }))?;

        let request = self
            .ctx
            .headers
            .iter()
            .fold(
                self.client.post(self.messages_url()),
                |request, (name, value)| request.header(name, value),
            )
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send();

        let response = match tokio::time::timeout(CALL_TIMEOUT, request).await {
            Ok(result) => result?,
            Err(_) => return Err(SummarizerError::Timeout),
        };

        match response.error_for_status_ref() {
            Ok(_) => extract_text(&response.bytes().await?),
            Err(_) => Err(SummarizerError::Status(response.status())),
        }
    }

    fn messages_url(&self) -> Url {
        let mut url = self.ctx.upstream.clone();
        url.set_path(MESSAGES_PATH);
        url
    }
}

fn extract_text(body: &[u8]) -> Result<String, SummarizerError> {
    Ok(serde_json::from_slice::<MessagesResponse>(body)?
        .content
        .into_iter()
        .map(|ContentBlock::Text { text }| text)
        .collect())
}

#[derive(serde::Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
}
