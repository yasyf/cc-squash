//! Integration tests for the L2 ON-PATH Interceptor (sub-phase 4d): a real axum app
//! on an ephemeral port, a real reqwest client driving it, and a mock upstream
//! (wiremock) standing in for `api.anthropic.com`. Each test pre-seeds a session's
//! [`SessionEcon`] — its staged plan, warm cache, and economics — so the Interceptor
//! runs DETERMINISTICALLY without a live summarizer, and asserts on the body the
//! upstream actually receives (the egress) plus the streamed response.
//!
//! Seeding flow: register the session, drive ONE warm-up request to lazy-init the
//! `econ` from a known model, wait for that request's off-path staging to settle,
//! then overwrite the staged plan + cache under the session lock and fire the
//! MEASURED request. The Interceptor consumes the staged plan synchronously before
//! `forward`, so the egress body reflects MY seeded plan regardless of the measured
//! request's own (post-forward) staging.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use ccs_core::{ChoiceTag, ModelId, SegmentKind, TokenCount};
use ccs_economics::CacheState;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_payload_bytes, segment_prompt, ContentDecision};
use ccs_proxy::config::RelayConfig;
use ccs_proxy::demux::{SessionCtx, SessionToken};
use ccs_proxy::staging::{StagedEntry, StagedPlan};
use ccs_proxy::{router, AppState};
use ccs_refs::{content_address, RefRecord, RefStore};
use reqwest::Url;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "claude-opus-4-8";
const AUTH_HEADER: &str = "x-api-key";
const AUTH_VALUE: &str = "sk-l2-test";

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

/// A canned `message_start` SSE the forwarded request gets back: a WARM cache hit
/// (creation small, read large) so the L0 breaker stays engaged.
fn warm_sse() -> String {
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\
     \"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":7,\
     \"cache_creation_input_tokens\":50,\"cache_read_input_tokens\":4000}}}\n\n\
     event: message_stop\ndata: {}\n\n"
        .to_owned()
}

/// A mock upstream that answers EVERY `/v1/messages` with the warm SSE. The
/// off-path summarizer (if it fires) gets the same — harmless, since we overwrite
/// the staged plan by hand and assert on the measured request's egress.
async fn warm_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(warm_sse()),
        )
        .mount(&upstream)
        .await;
    upstream
}

async fn spawn_proxy(upstream: &str) -> (SocketAddr, AppState) {
    let state = AppState::with_upstream(Url::parse(upstream).expect("url"), test_store().await)
        .expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (addr, state)
}

fn register(state: &AppState, token: &str, config: RelayConfig) {
    state.sessions.insert(
        SessionToken(token.to_owned()),
        SessionCtx {
            config,
            session_id: ccs_core::SessionId::new(token),
            econ: None,
        },
    );
}

async fn post(proxy: SocketAddr, token: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}/s/{token}/v1/messages"))
        .header("content-type", "application/json")
        .header(AUTH_HEADER, AUTH_VALUE)
        .body(body)
        .send()
        .await
        .expect("send")
}

/// A body whose FIRST assistant turn is a long historical string segment — the L2
/// squash candidate (an `AssistantTurn` is never salience-pinned, several later
/// turns push it out of the recency window, and its content is a JSON string so the
/// safe single-message-string splice target applies). Above the 256-char pre-gate.
///
/// A large `system` block keeps the POST-squash cacheable prefix well above the
/// model's `min_cache_floor` (1024 tokens) without bloating the SUFFIX after the
/// candidate — the later turns are short, so the bust cost of re-caching that suffix
/// is small and the squash's NPV is comfortably positive on a warm cache.
fn squashable_body() -> Vec<u8> {
    let system = "you are a meticulous engineering assistant with deep context. ".repeat(90);
    let long = "the assistant explained a great deal of detailed context here. ".repeat(20);
    serde_json::json!({
        "model": MODEL,
        "max_tokens": 4096,
        "system": system,
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

/// A body whose FIRST historical segment is a real client `tool_use` / `tool_result`
/// pair (array content) — the L2 squash target is the user message's `tool_result`
/// block, not the assistant `tool_use`. The result content is long enough to clear
/// the span floor; several later turns push the pair out of the recency window. A
/// large `system` keeps the post-squash cacheable prefix above the model floor.
fn tool_pair_body() -> Vec<u8> {
    let system = "you are a meticulous engineering assistant with deep context. ".repeat(90);
    let output = "the tool printed a great deal of detailed output here. ".repeat(20);
    serde_json::json!({
        "model": MODEL,
        "max_tokens": 4096,
        "system": system,
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

/// The content-address ref_id + a `RefRecord` for the squash candidate — the FIRST
/// (long, historical) assistant turn within `body`, found by kind rather than a
/// fixed index so a leading `system`/`tools` segment doesn't shift it.
fn candidate_ref(body_bytes: &[u8], session: &str) -> (ccs_core::RefId, RefRecord) {
    candidate_ref_of(body_bytes, session, SegmentKind::AssistantTurn)
}

/// The content-address ref_id + a `RefRecord` for the first segment of `kind` — the
/// staged-plan key is the WHOLE-segment payload (for a `ToolPair`, the assistant
/// `tool_use` plus the user `tool_result`).
fn candidate_ref_of(
    body_bytes: &[u8],
    session: &str,
    kind: SegmentKind,
) -> (ccs_core::RefId, RefRecord) {
    let body = parse_body(body_bytes).expect("parse");
    let segs = segment_prompt(&body);
    let seg = segs
        .iter()
        .find(|s| s.kind == kind)
        .expect("a candidate of the requested kind");
    let payload = segment_payload_bytes(seg, &body);
    let ref_id = content_address(&payload);
    let rec = RefRecord {
        ref_id: ref_id.clone(),
        byte_len: payload.len() as u64,
        token_estimate: seg.token_estimate,
        source_uuid: seg
            .source_uuids
            .first()
            .cloned()
            .unwrap_or_else(|| ccs_core::MessageId::new("0")),
        session_id: ccs_core::SessionId::new(session),
        kind: seg.kind,
        created_at: 0.0,
    };
    (ref_id, rec)
}

/// A staged plan with the single candidate, a `compress` (reversible-ref) decision.
fn staged_plan(ref_id: ccs_core::RefId, rec: RefRecord, summary: &str) -> StagedPlan {
    let mut by_content = HashMap::new();
    by_content.insert(
        ref_id,
        StagedEntry {
            rec,
            decision: ContentDecision {
                choice: ChoiceTag::Compress,
                ranges_to_keep: Vec::new(),
                summary_content: Some(summary.to_owned()),
            },
        },
    );
    StagedPlan { by_content }
}

/// Wall-clock seconds since the epoch — the proxy folds the cache warmth model
/// against this same clock, so a seeded cache must be timestamped near it to read
/// warm.
fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

/// A warm cache state for `model` with a large positive prefix and a RECENT
/// timestamp, so the cache reads warm (not cold) and a flush at the candidate's
/// offset clears the NPV floor.
fn warm_cache(model: &str) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(8000),
        last_request_ts: now_s(),
        assumed_ttl_s: 3600.0,
        model: ModelId::new(model),
        breakpoints: Vec::new(),
    }
}

/// Lazy-init `econ` by driving one warm-up request, then settle.
async fn warmup(proxy: SocketAddr, token: &str) {
    let resp = post(proxy, token, squashable_body()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");
    // Let the L0 drain + any L1 staging settle so a stale stage_next can't clobber
    // the plan we seed next.
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// Lock the session's econ and apply `mutate`, returning whether the session existed.
fn seed<F: FnOnce(&mut ccs_proxy::session::SessionEcon)>(
    state: &AppState,
    token: &str,
    mutate: F,
) -> bool {
    let econ = state
        .sessions
        .get(&SessionToken(token.to_owned()))
        .and_then(|c| c.econ.clone());
    match econ {
        Some(econ) => {
            let mut guard = econ.lock().expect("lock");
            mutate(&mut guard);
            true
        }
        None => false,
    }
}

#[tokio::test]
async fn live_squash_applies_staged_plan() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-live", RelayConfig::default());
    warmup(proxy, "tok-live").await;

    let body = squashable_body();
    let (ref_id, rec) = candidate_ref(&body, "tok-live");
    let ref_str = ref_id.as_str().to_owned();
    assert!(
        seed(&state, "tok-live", |e| {
            e.cache = warm_cache(MODEL);
            e.intercept_enabled = true;
            e.remaining_turns = 50.0;
            e.staged = Some(staged_plan(ref_id, rec, "condensed early context"));
        }),
        "session seeded",
    );

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-live", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    // The measured request is the next non-summarizer POST after `before`.
    let measured = &reqs[before];
    assert!(
        measured.body.len() < body.len(),
        "the egress body must be SHRUNK ({} >= {})",
        measured.body.len(),
        body.len(),
    );
    let egress = String::from_utf8_lossy(&measured.body);
    assert!(
        egress.contains(&format!("ref={ref_str}")),
        "the egress body carries the placeholder ref marker",
    );
    assert!(
        egress.contains("condensed early context"),
        "the placeholder carries the staged summary",
    );
}

#[tokio::test]
async fn live_squash_replaces_tool_result_block() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-pair", RelayConfig::default());
    warmup(proxy, "tok-pair").await;

    let body = tool_pair_body();
    let (ref_id, rec) = candidate_ref_of(&body, "tok-pair", SegmentKind::ToolPair);
    let ref_str = ref_id.as_str().to_owned();
    assert!(
        seed(&state, "tok-pair", |e| {
            e.cache = warm_cache(MODEL);
            e.intercept_enabled = true;
            e.remaining_turns = 50.0;
            e.staged = Some(staged_plan(ref_id, rec, "condensed tool output"));
        }),
        "session seeded",
    );

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-pair", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    let measured = &reqs[before];
    assert!(
        measured.body.len() < body.len(),
        "the egress body must be SHRUNK ({} >= {})",
        measured.body.len(),
        body.len(),
    );
    let egress: serde_json::Value =
        serde_json::from_slice(&measured.body).expect("egress is valid JSON");
    let result_block = &egress["messages"][2]["content"][0];
    assert_eq!(
        result_block["type"], "tool_result",
        "the squashed block stays a tool_result",
    );
    assert_eq!(
        result_block["tool_use_id"], "tu_1",
        "the tool_use_id is preserved so the pair stays intact",
    );
    assert!(
        result_block["content"]
            .as_str()
            .expect("content is a string")
            .contains(&format!("ref={ref_str}")),
        "the tool_result content is the placeholder carrying the ref marker",
    );
    assert_eq!(
        egress["messages"][1]["content"][0]["type"], "tool_use",
        "the assistant tool_use block is untouched",
    );
    assert_eq!(
        egress["messages"][1]["content"][0]["id"], "tu_1",
        "the tool_use id is byte-intact",
    );
}

#[tokio::test]
async fn hold_forwards_identity() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-hold", RelayConfig::default());
    warmup(proxy, "tok-hold").await;

    let body = squashable_body();
    let (ref_id, rec) = candidate_ref(&body, "tok-hold");
    // A WARM cache on the SAME model (no free Cold/ModelSwitch bust) plus a high NPV
    // floor no warm flush clears: the controller holds `WarmDeep`, so the original
    // forwards unchanged.
    seed(&state, "tok-hold", |e| {
        e.cache = warm_cache(MODEL);
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.npv_floor = 1_000.0; // an unreachable bar — every flush is sub-floor NPV
        e.staged = Some(staged_plan(ref_id, rec, "summary"));
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-hold", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    assert_eq!(
        reqs[before].body, body,
        "a Hold must forward the ORIGINAL bytes unchanged",
    );
}

#[tokio::test]
async fn refhot_prefilter_holds() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-hot", RelayConfig::default());
    warmup(proxy, "tok-hot").await;

    let body = squashable_body();
    let (ref_id, rec) = candidate_ref(&body, "tok-hot");
    let hot = ref_id.clone();
    seed(&state, "tok-hot", |e| {
        e.cache = warm_cache(MODEL);
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.staged = Some(staged_plan(ref_id, rec, "summary"));
        // The only candidate's ref is hot: the RefHot pre-filter empties the batch.
        e.hot_refs = HashSet::from([hot]);
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-hot", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    assert_eq!(
        reqs[before].body, body,
        "a RefHot pre-filter must forward the ORIGINAL bytes (no splice, no materialize)",
    );
}

#[tokio::test]
async fn model_switch_rides_bust() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-switch", RelayConfig::default());
    warmup(proxy, "tok-switch").await;

    let body = squashable_body();
    let (ref_id, rec) = candidate_ref(&body, "tok-switch");
    let ref_str = ref_id.as_str().to_owned();
    // Cache warmed on a DIFFERENT model than the egress body's: a free model-switch
    // bust the controller rides (RideFreeBust), so the squash applies at zero
    // marginal cost even with a short horizon.
    seed(&state, "tok-switch", |e| {
        e.cache = warm_cache("claude-sonnet-4-5");
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.staged = Some(staged_plan(ref_id, rec, "switch summary"));
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-switch", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    let egress = String::from_utf8_lossy(&reqs[before].body);
    assert!(
        reqs[before].body.len() < body.len() && egress.contains(&format!("ref={ref_str}")),
        "a model switch rides the free bust and applies the squash",
    );
}

#[tokio::test]
async fn unknown_model_disables_intercept() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-unknown", RelayConfig::default());

    // A body with an UNKNOWN model: `economics_for` returns None at lazy-init, so
    // `econ.econ` is None and the Interceptor bails to identity — even with a plan.
    let unknown = serde_json::json!({
        "model": "totally-made-up-model",
        "max_tokens": 4096,
        "messages": [
            {"role": "user", "content": "kick off the work"},
            {"role": "assistant", "content": "the assistant explained a great deal of detailed context here. ".repeat(20)},
            {"role": "user", "content": "second turn"},
            {"role": "assistant", "content": "second reply"},
            {"role": "user", "content": "third turn"},
            {"role": "assistant", "content": "third reply"},
            {"role": "user", "content": "fourth turn that is current"},
        ],
    })
    .to_string()
    .into_bytes();

    let resp = post(proxy, "tok-unknown", unknown.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (ref_id, rec) = candidate_ref(&unknown, "tok-unknown");
    seed(&state, "tok-unknown", |e| {
        // Even after force-seeding a warm cache + plan, econ.econ stays None.
        e.cache = warm_cache("totally-made-up-model");
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.staged = Some(staged_plan(ref_id, rec, "summary"));
        assert!(e.econ.is_none(), "an unknown model leaves econ None");
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-unknown", unknown.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    assert_eq!(
        reqs[before].body, unknown,
        "an unknown model disables interception — identity forward",
    );
}

#[tokio::test]
async fn breaker_reverts_on_overbust() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-break", RelayConfig::default());
    warmup(proxy, "tok-break").await;

    // Simulate a rewrite that mispriced the cache: a tiny predicted bust, then a
    // realized creation far past it. Past warmup, the breaker trips and disables
    // interception, so the next turn is identity even with a valid staged plan.
    let body = squashable_body();
    let (ref_id, rec) = candidate_ref(&body, "tok-break");
    seed(&state, "tok-break", |e| {
        e.cache = warm_cache(MODEL);
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.turn = 5; // past warmup
        e.last_predicted_bust = Some(ccs_economics::Cost {
            dollars: 0.0,
            tokens: TokenCount(50),
        });
        // A realized creation ~10x the prediction trips the breaker.
        e.observe(
            ccs_economics::CacheUsage {
                cache_creation_input_tokens: TokenCount(2000),
                cache_read_input_tokens: TokenCount(100),
                input_tokens: TokenCount(10),
            },
            2_000_000.0,
        );
        assert!(!e.intercept_enabled, "the overbust must trip the breaker");
        // Re-seed a valid plan; the disabled breaker must still force identity.
        e.staged = Some(staged_plan(ref_id, rec, "summary"));
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-break", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    assert_eq!(
        reqs[before].body, body,
        "a tripped breaker forces identity on the next turn",
    );
}

#[tokio::test]
async fn deterministic_fallback_overbudget() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-fallback", RelayConfig::default());
    warmup(proxy, "tok-fallback").await;

    // No staged plan, but the LAST segment is huge — soft_pressure is OverBudget on
    // a small window, so the deterministic fallback strips/drops historical
    // segments. Pinned/recency segments stay; the body still shrinks and gate-valid.
    let huge_current = "current turn with a very large payload that blows the budget. ".repeat(80);
    let history = "an old assistant reply with plenty of detail to drop. ".repeat(30);
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "kick off the work for the fallback test"},
            {"role": "assistant", "content": history},
            {"role": "user", "content": "second turn here"},
            {"role": "assistant", "content": "second reply with some content"},
            {"role": "user", "content": "third turn here"},
            {"role": "assistant", "content": "third reply with some content"},
            {"role": "user", "content": huge_current},
        ],
    })
    .to_string()
    .into_bytes();

    seed(&state, "tok-fallback", |e| {
        e.cache = warm_cache(MODEL);
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.staged = None; // force the deterministic fallback
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-fallback", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    let egress = String::from_utf8_lossy(&reqs[before].body);
    assert!(
        reqs[before].body.len() < body.len(),
        "the deterministic fallback must shrink the over-budget body",
    );
    // The current (last) turn is pinned/recency-protected — its payload survives.
    assert!(
        egress.contains("current turn with a very large payload"),
        "the pinned current turn must be untouched by the fallback",
    );
}

#[tokio::test]
async fn deterministic_fallback_shrinks_tool_output() {
    let upstream = warm_upstream().await;
    let (proxy, state) = spawn_proxy(&upstream.uri()).await;
    register(&state, "tok-fb-tool", RelayConfig::default());
    warmup(proxy, "tok-fb-tool").await;

    // Over-budget, no staged plan: the historical block is a client tool_use /
    // tool_result pair. The string-only fallback could never touch it; routing
    // through squash_targets drops the tool_result content while keeping the pair.
    let huge_current = "current turn with a very large payload that blows the budget. ".repeat(80);
    let tool_output = "an old tool dumped a great deal of output to drop. ".repeat(30);
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "kick off the work for the fallback test"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"cmd": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": tool_output}
            ]},
            {"role": "user", "content": "second turn here"},
            {"role": "assistant", "content": "second reply with some content"},
            {"role": "user", "content": "third turn here"},
            {"role": "assistant", "content": "third reply with some content"},
            {"role": "user", "content": huge_current},
        ],
    })
    .to_string()
    .into_bytes();

    seed(&state, "tok-fb-tool", |e| {
        e.cache = warm_cache(MODEL);
        e.intercept_enabled = true;
        e.remaining_turns = 50.0;
        e.staged = None;
    });

    let before = upstream.received_requests().await.expect("reqs").len();
    let resp = post(proxy, "tok-fb-tool", body.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain");

    let reqs = upstream.received_requests().await.expect("reqs");
    assert!(
        reqs[before].body.len() < body.len(),
        "the deterministic fallback must shrink the tool-output-heavy body",
    );
    let egress: serde_json::Value =
        serde_json::from_slice(&reqs[before].body).expect("egress is valid JSON");
    let result_block = &egress["messages"][2]["content"][0];
    assert_eq!(
        result_block["type"], "tool_result",
        "the dropped block stays a tool_result",
    );
    assert_eq!(
        result_block["tool_use_id"], "tu_1",
        "the irreversible drop keeps tool_use_id so the pair stays intact",
    );
    assert_eq!(
        egress["messages"][1]["content"][0]["type"], "tool_use",
        "the assistant tool_use block survives the drop",
    );
}
