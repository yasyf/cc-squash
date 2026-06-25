//! Mocked-boundary tests for the off-path summarizer: the Anthropic upstream is a
//! `wiremock` server, the agent (client + decision + folder) stays real.

use ccs_core::ChoiceTag;
use ccs_policy::{PolicyConfig, WorkingState};
use ccs_summarizer::{decide, fold, SessionAuthContext, SummarizerClient};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AUTH_HEADER: &str = "x-api-key";
const AUTH_VALUE: &str = "sk-test";

fn auth_context(upstream: &str) -> SessionAuthContext {
    SessionAuthContext {
        headers: vec![(
            HeaderName::from_static(AUTH_HEADER),
            HeaderValue::from_static(AUTH_VALUE),
        )],
        upstream: Url::parse(upstream).expect("valid upstream url"),
    }
}

fn anthropic_text(text: &str) -> serde_json::Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
    })
}

async fn mock_with_text(text: &str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_text(text)))
        .mount(&upstream)
        .await;
    upstream
}

fn long_content() -> String {
    "lorem ipsum dolor sit amet ".repeat(20)
}

#[tokio::test]
async fn decide_parses_canned_json() {
    let upstream = mock_with_text(r#"{"choice":"summarize","summary_content":"condensed"}"#).await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    let decision = decide(
        &client,
        &long_content(),
        &["NARRATIVE"],
        &PolicyConfig::default(),
    )
    .await;

    assert_eq!(decision.choice, ChoiceTag::Summarize);
    assert_eq!(decision.summary_content.as_deref(), Some("condensed"));
}

#[tokio::test]
async fn decide_extracts_json_amid_prose() {
    let upstream = mock_with_text(
        "Here is my decision:\n{\"choice\":\"truncate\",\"ranges_to_keep\":[{\"start\":1,\"end\":3}]}\nDone.",
    )
    .await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    let decision = decide(&client, &long_content(), &[], &PolicyConfig::default()).await;

    assert_eq!(decision.choice, ChoiceTag::Truncate);
    assert_eq!(decision.ranges_to_keep.len(), 1);
}

#[tokio::test]
async fn normalize_self_repair() {
    let truncate_no_ranges = mock_with_text(r#"{"choice":"truncate"}"#).await;
    let client = SummarizerClient::new(auth_context(&truncate_no_ranges.uri()));
    assert_eq!(
        decide(&client, &long_content(), &[], &PolicyConfig::default())
            .await
            .choice,
        ChoiceTag::Keep,
        "truncate without ranges self-repairs to Keep",
    );

    let summarize_no_content = mock_with_text(r#"{"choice":"summarize"}"#).await;
    let client = SummarizerClient::new(auth_context(&summarize_no_content.uri()));
    assert_eq!(
        decide(&client, &long_content(), &[], &PolicyConfig::default())
            .await
            .choice,
        ChoiceTag::Compress,
        "summarize without content self-repairs to Compress",
    );
}

#[tokio::test]
async fn fail_to_keep_on_500() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    assert_eq!(
        decide(&client, &long_content(), &[], &PolicyConfig::default())
            .await
            .choice,
        ChoiceTag::Keep,
    );
}

#[tokio::test]
async fn pregate_zero_http() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    let decision = decide(&client, "tiny", &[], &PolicyConfig::default()).await;

    assert_eq!(decision.choice, ChoiceTag::Keep);
    let received = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert!(received.is_empty(), "pre-gate must issue zero HTTP calls");
}

#[tokio::test]
async fn request_carries_model_and_auth() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header(AUTH_HEADER, AUTH_VALUE))
        .and(body_string_contains("claude-sonnet-4-6"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(anthropic_text(r#"{"choice":"keep"}"#)),
        )
        .mount(&upstream)
        .await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    decide(&client, &long_content(), &[], &PolicyConfig::default()).await;

    let received = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(
        received[0].headers.get(AUTH_HEADER).expect("auth header"),
        AUTH_VALUE,
    );
}

#[tokio::test]
async fn rsum_fold_reconciles() {
    let folded = json!({
        "constraints": [
            {
                "text": "Always plan before editing; never touch secrets.",
                "source_message": "uuid-1",
                "superseded_by": null
            },
            {
                "text": "Use the staging database.",
                "source_message": "uuid-2",
                "superseded_by": "uuid-9"
            }
        ],
        "decisions": [
            {"text": "Port bioqa verbatim.", "rationale": "battle-tested", "planned": false}
        ],
        "in_flight": {
            "task": "implement folder",
            "last_safe_point": "client.rs green",
            "open_files": ["src/folder.rs"],
            "skill_paths": ["CLAUDE.md"]
        }
    });
    let upstream = mock_with_text(&folded.to_string()).await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    let state = fold(
        &client,
        &WorkingState::default(),
        "the user added a new constraint",
    )
    .await;

    assert_eq!(state.constraints.len(), 2);
    assert_eq!(
        state.constraints[0].text, "Always plan before editing; never touch secrets.",
        "live constraint preserved verbatim",
    );
    assert!(state.constraints[0].superseded_by.is_none());
    assert_eq!(
        state.constraints[1]
            .superseded_by
            .as_ref()
            .map(|m| m.as_str()),
        Some("uuid-9"),
        "superseded constraint carries superseded_by",
    );
    assert!(!state.decisions[0].planned);
    assert_eq!(
        state.in_flight.as_ref().map(|w| w.skill_paths.as_slice()),
        Some(["CLAUDE.md".to_string()].as_slice()),
    );
}

#[tokio::test]
async fn fold_fails_safe_to_prev() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let client = SummarizerClient::new(auth_context(&upstream.uri()));

    let prev = WorkingState::default();
    assert_eq!(fold(&client, &prev, "new turns").await, prev);
}
