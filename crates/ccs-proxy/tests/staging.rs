//! Integration tests for L1 OFF-PATH staging (sub-phase 4c): a real axum app on
//! an ephemeral port, a real reqwest client driving it, and one mock upstream
//! standing in for BOTH `api.anthropic.com` (the forwarded request) and the
//! off-path summarizer (which replays the captured auth to that same upstream).
//!
//! The summarizer's `/v1/messages` call is told apart from the forwarded request
//! by its pinned `claude-sonnet-4-6` model; the forwarded request takes the
//! Forward branch and gets a canned streaming `message_start` response.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ccs_proxy::config::RelayConfig;
use ccs_proxy::demux::{SessionCtx, SessionToken};
use ccs_proxy::{router, AppState};
use ccs_refs::RefStore;
use ccs_summarizer::SUMMARIZER_MODEL;
use reqwest::Url;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SUMMARIZER_DELAY: Duration = Duration::from_millis(400);
const AUTH_HEADER: &str = "x-api-key";
const AUTH_VALUE: &str = "sk-stage-test";
const BETA_HEADER: &str = "anthropic-beta";
const BETA_VALUE: &str = "fine-grained-tool-streaming-2025";

/// An ephemeral refs store under a process-lifetime temp dir; each call gets its
/// own db file.
async fn test_store() -> Arc<RefStore> {
    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().expect("temp dir"));
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);

    let path = TEST_DIR.path().join(format!(
        "refs-{}.db",
        DB_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    Arc::new(RefStore::open(path).await.expect("open refs db"))
}

/// A canned `message_start` SSE the forwarded request gets back, so the L0 tap and
/// the response both resolve cleanly.
fn message_start_sse() -> String {
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\
     \"model\":\"claude-opus-4-20250514\",\"usage\":{\"input_tokens\":7,\
     \"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":250}}}\n\n\
     event: message_stop\ndata: {}\n\n"
        .to_owned()
}

/// A summarizer JSON reply wrapped in the Anthropic `messages` envelope.
fn summarizer_reply(json_text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_sum",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": json_text}],
    })
}

/// A mock upstream that answers the off-path summarizer's `/v1/messages` (any body
/// carrying the pinned summarizer model) with `reply` after `delay`, and every
/// other `/v1/messages` (the forwarded request) with the canned SSE.
async fn mock_upstream(reply: serde_json::Value, delay: Duration) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains(SUMMARIZER_MODEL))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(reply),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(message_start_sse()),
        )
        .mount(&upstream)
        .await;
    upstream
}

async fn spawn_proxy_with_state(upstream: &str) -> (SocketAddr, AppState) {
    let state = AppState::with_upstream(
        Url::parse(upstream).expect("upstream url"),
        test_store().await,
    )
    .expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    let app = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    (addr, state)
}

fn register(state: &AppState, token: &str) {
    state.sessions.insert(
        SessionToken(token.to_owned()),
        SessionCtx {
            config: RelayConfig::default(),
            session_id: ccs_core::SessionId::new(token),
            econ: None,
        },
    );
}

/// A non-compaction body whose FIRST assistant turn is a long historical segment —
/// a squash candidate: an `AssistantTurn` is never salience-pinned and never
/// true-human, and several later turns push it out of the recency window. Its
/// payload is above the 256-char pre-gate so it reaches the summarizer LLM. (A
/// true-human `UserTurn` is verbatim-pinned, so it would never be a candidate.)
fn squashable_body() -> Vec<u8> {
    let long = "the assistant explained a lot of detailed context in this reply. ".repeat(12);
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "kick off the work"},
            {"role": "assistant", "content": long},
            {"role": "user", "content": "second turn"},
            {"role": "assistant", "content": "second reply"},
            {"role": "user", "content": "third turn"},
            {"role": "assistant", "content": "third reply"},
            {"role": "user", "content": "fourth turn that is current"},
        ],
    })
    .to_string()
    .into_bytes()
}

/// A non-compaction body whose FIRST historical segment is a client `tool_use` /
/// `tool_result` pair (array content). The `tool_result` content is above the
/// pre-gate so the staging summarizer scores it; later turns push the pair out of
/// the recency window so it is a squash candidate.
fn tool_pair_body() -> Vec<u8> {
    let output = "the tool printed a great deal of detailed output here. ".repeat(12);
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "kick off the work"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"cmd": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": output}
            ]},
            {"role": "user", "content": "second turn"},
            {"role": "assistant", "content": "second reply"},
            {"role": "user", "content": "third turn"},
            {"role": "assistant", "content": "third reply"},
            {"role": "user", "content": "fourth turn that is current"},
        ],
    })
    .to_string()
    .into_bytes()
}

/// Like [`squashable_body`], but with a `role: "system"` string-content message
/// (the SessionStart-hook / deferred-tools reminder Claude Code injects into
/// `messages[]`) interleaved among the turns. Before the `Role::System` fix this
/// made `parse_body` fail, silently disabling the whole staging engine. The long
/// first assistant turn is still an old, unpinned squash candidate.
fn system_role_body() -> Vec<u8> {
    let long = "the assistant explained a lot of detailed context in this reply. ".repeat(12);
    let reminder = "<system-reminder> SessionStart hook context and deferred-tools notice. "
        .repeat(8);
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "kick off the work"},
            {"role": "assistant", "content": long},
            {"role": "system", "content": reminder},
            {"role": "user", "content": "second turn"},
            {"role": "assistant", "content": "second reply"},
            {"role": "user", "content": "third turn"},
            {"role": "assistant", "content": "third reply"},
            {"role": "user", "content": "fourth turn that is current"},
        ],
    })
    .to_string()
    .into_bytes()
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

async fn post(proxy: SocketAddr, token: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}/s/{token}/v1/messages"))
        .header("content-type", "application/json")
        .header(AUTH_HEADER, AUTH_VALUE)
        .header(BETA_HEADER, BETA_VALUE)
        .body(body)
        .send()
        .await
        .expect("send")
}

/// Poll the session's staged plan until it is `Some` or the deadline passes. The
/// `SessionEcon` `Arc` is cloned out before any `.await`, so no DashMap ref or
/// mutex guard is held across the sleep.
async fn await_staged(state: &AppState, token: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let econ = state
            .sessions
            .get(&SessionToken(token.to_owned()))
            .and_then(|ctx| ctx.econ.clone());
        if let Some(econ) = econ {
            if econ.lock().expect("lock").staged.is_some() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn stage_next_produces_staged_plan() {
    let reply =
        summarizer_reply(r#"{"choice":"summarize","summary_content":"condensed early context"}"#);
    let upstream = mock_upstream(reply, Duration::ZERO).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-stage");

    let resp = post(proxy, "tok-stage", squashable_body()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain body");

    assert!(
        await_staged(&state, "tok-stage").await,
        "the spawned staging task must populate `staged`",
    );

    let ctx = state
        .sessions
        .get(&SessionToken("tok-stage".to_owned()))
        .expect("session");
    let econ = ctx.econ.as_ref().expect("econ");
    let guard = econ.lock().expect("lock");
    let plan = guard.staged.as_ref().expect("staged plan");
    assert!(
        !plan.by_content.is_empty(),
        "a squashable segment with a non-Keep decision must produce a staged entry",
    );
    let entry = plan.by_content.values().next().expect("entry");
    assert_eq!(
        entry.decision.choice,
        ccs_core::ChoiceTag::Summarize,
        "the staged entry carries the summarizer's choice",
    );
    assert_eq!(
        entry.rec.ref_id.as_str(),
        plan.by_content.keys().next().expect("key").as_str(),
        "the plan is keyed by the entry's content-address ref_id",
    );
}

#[tokio::test]
async fn stage_next_stages_tool_pair_segment() {
    let reply =
        summarizer_reply(r#"{"choice":"summarize","summary_content":"condensed tool output"}"#);
    let upstream = mock_upstream(reply, Duration::ZERO).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-pair-stage");

    let resp = post(proxy, "tok-pair-stage", tool_pair_body()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain body");

    assert!(
        await_staged(&state, "tok-pair-stage").await,
        "the spawned staging task must populate `staged`",
    );

    let ctx = state
        .sessions
        .get(&SessionToken("tok-pair-stage".to_owned()))
        .expect("session");
    let econ = ctx.econ.as_ref().expect("econ");
    let guard = econ.lock().expect("lock");
    let plan = guard.staged.as_ref().expect("staged plan");
    assert!(
        plan.by_content
            .values()
            .any(|e| e.rec.kind == ccs_core::SegmentKind::ToolPair),
        "the tool_use/tool_result pair must be staged as a ToolPair entry",
    );
}

#[tokio::test]
async fn stage_next_stages_body_containing_system_role_message() {
    // A body carrying a `role: "system"` reminder must flow end-to-end through
    // parse → segment → decide → store.put → commit. Before the `Role::System`
    // fix, `parse_body` errored on the `system` variant and NOTHING staged.
    let reply =
        summarizer_reply(r#"{"choice":"summarize","summary_content":"condensed early context"}"#);
    let upstream = mock_upstream(reply, Duration::ZERO).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-system");

    let resp = post(proxy, "tok-system", system_role_body()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain body");

    assert!(
        await_staged(&state, "tok-system").await,
        "a body with a system-role message must still stage a plan",
    );

    let ctx = state
        .sessions
        .get(&SessionToken("tok-system".to_owned()))
        .expect("session");
    let econ = ctx.econ.as_ref().expect("econ");
    let ref_id = {
        let guard = econ.lock().expect("lock");
        let plan = guard.staged.as_ref().expect("staged plan");
        assert!(
            !plan.by_content.is_empty(),
            "the squashable segment must produce a staged entry",
        );
        plan.by_content
            .values()
            .next()
            .expect("entry")
            .rec
            .ref_id
            .clone()
    };

    // The original was `put` into the REAL RefStore under this session: the staged
    // ref resolves, proving store.put ran on the system-role-carrying body.
    let resolved = state
        .store
        .retrieve(
            &ref_id,
            &ccs_core::SessionId::new("tok-system"),
            None,
            now_s(),
        )
        .await
        .expect("retrieve");
    assert!(
        matches!(resolved, ccs_refs::RetrieveResult::Hit { .. }),
        "stage_next must have stored the original under the staged ref",
    );
}

#[tokio::test]
async fn staging_never_delays_response() {
    // The summarizer mock is slow; the forwarded response must return well before
    // the summarizer could possibly finish, proving staging is off-path.
    let reply = summarizer_reply(r#"{"choice":"keep"}"#);
    let upstream = mock_upstream(reply, SUMMARIZER_DELAY).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-fast");

    let start = Instant::now();
    let resp = post(proxy, "tok-fast", squashable_body()).await;
    let status = resp.status();
    let elapsed = start.elapsed();
    let _ = resp.bytes().await.expect("drain body");

    assert_eq!(status, 200);
    assert!(
        elapsed < SUMMARIZER_DELAY,
        "response latency {elapsed:?} must be well under the summarizer delay \
         {SUMMARIZER_DELAY:?} — staging cannot block the hot path",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_no_deadlock() {
    let reply = summarizer_reply(r#"{"choice":"summarize","summary_content":"condensed"}"#);
    let upstream = mock_upstream(reply, Duration::from_millis(100)).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-a");
    register(&state, "tok-b");

    let (ra, rb) = tokio::join!(
        post(proxy, "tok-a", squashable_body()),
        post(proxy, "tok-b", squashable_body()),
    );
    assert_eq!(ra.status(), 200);
    assert_eq!(rb.status(), 200);
    let _ = ra.bytes().await.expect("drain a");
    let _ = rb.bytes().await.expect("drain b");

    assert!(
        await_staged(&state, "tok-a").await,
        "session a stages without deadlock",
    );
    assert!(
        await_staged(&state, "tok-b").await,
        "session b stages concurrently without deadlock",
    );
}

#[tokio::test]
async fn auth_captured_from_first_request() {
    let reply = summarizer_reply(r#"{"choice":"summarize","summary_content":"condensed"}"#);
    let upstream = mock_upstream(reply, Duration::ZERO).await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-auth");

    let resp = post(proxy, "tok-auth", squashable_body()).await;
    let _ = resp.bytes().await.expect("drain body");
    assert!(await_staged(&state, "tok-auth").await, "staging completes");

    // The summarizer replayed the captured auth + beta headers verbatim on its own
    // `/v1/messages` calls (the ones carrying the pinned summarizer model).
    let received = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    let summarizer_calls: Vec<_> = received
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains(SUMMARIZER_MODEL))
        .collect();
    assert!(
        !summarizer_calls.is_empty(),
        "the summarizer must have issued at least one off-path call",
    );
    for call in &summarizer_calls {
        assert_eq!(
            call.headers.get(AUTH_HEADER).expect("auth header replayed"),
            AUTH_VALUE,
            "the captured authorization header is replayed verbatim",
        );
        assert_eq!(
            call.headers.get(BETA_HEADER).expect("beta header replayed"),
            BETA_VALUE,
            "the captured anthropic-beta header is replayed verbatim",
        );
    }
}
