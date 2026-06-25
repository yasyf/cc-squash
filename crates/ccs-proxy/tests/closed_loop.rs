//! The CLOSED-LOOP proof: L1 `stage_next` PRODUCES the squash plan organically (a
//! real summarizer decision, a real `RefStore` put, a real `SessionEcon`), then L2
//! `intercept::run` APPLIES that same plan on the next turn — no hand-seeded
//! `StagedPlan` anywhere. The only mock is the Anthropic boundary (a `wiremock`
//! upstream the summarizer replays its captured auth to), exactly as the
//! `ccs-summarizer` tests mock it; everything between `stage_next` and the egress
//! rewrite is the real path.
//!
//! Loop shape, mirroring `relay::serve`:
//!   turn N   — `stage_next(econ, body, …)` scores the tool_result, the mocked
//!              summarizer returns `Compress`, the original is `put` into the store,
//!              and `econ.staged` is populated (the L1 → L2 handoff).
//!   turn N+1 — `intercept_inputs(econ)` snapshots that plan (CONSUMING it via
//!              `take`, one apply per turn — the same `staged.take()` the real
//!              `relay::intercept_inputs` does), `intercept::run` splices the
//!              tool_result into a placeholder, the validity gate accepts the
//!              shrink, and the egress carries the ref the store still resolves.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use ccs_core::{ChoiceTag, ModelId, SegmentKind, SessionId, TokenCount};
use ccs_economics::CacheState;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_payload_bytes, segment_prompt, PolicyConfig};
use ccs_proxy::intercept::{self, InterceptInputs};
use ccs_proxy::session::SessionEcon;
use ccs_proxy::staging::stage_next;
use ccs_refs::{content_address, RefStore, RetrieveResult};
use ccs_summarizer::{SessionAuthContext, SUMMARIZER_MODEL};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "claude-opus-4-8";
const AUTH_HEADER: &str = "x-api-key";
const AUTH_VALUE: &str = "sk-closed-loop";

/// An ephemeral refs store under a process-lifetime temp dir; each call gets its
/// own db file so concurrent tests never share state.
async fn test_store() -> Arc<RefStore> {
    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().expect("temp dir"));
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);
    let path = TEST_DIR.path().join(format!(
        "refs-{}.db",
        DB_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    Arc::new(RefStore::open(path).await.expect("open refs db"))
}

/// A summarizer reply wrapped in the Anthropic `messages` envelope — the decision
/// JSON is the model's `content` text, exactly the shape `decide`'s tolerant parser
/// consumes.
fn summarizer_reply(json_text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_sum",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": json_text}],
    })
}

/// A mock Anthropic upstream that answers the off-path summarizer's `/v1/messages`
/// (told apart by the pinned summarizer model in its body) with a deterministic
/// `compress` decision — the reversible-ref strategy, so the placeholder keeps the
/// `retrieve(...)` affordance and the store round-trips. No live LLM, no network
/// beyond the loopback wiremock.
async fn mock_summarizer(summary: &str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains(SUMMARIZER_MODEL))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(summarizer_reply(&format!(
                r#"{{"choice":"compress","summary_content":"{summary}"}}"#
            ))),
        )
        .mount(&upstream)
        .await;
    upstream
}

/// The captured auth context the summarizer replays — its `upstream` points at the
/// wiremock, so `stage_next`'s real `SummarizerClient` calls the mock.
fn auth(upstream: &str) -> SessionAuthContext {
    SessionAuthContext {
        headers: vec![(
            HeaderName::from_static(AUTH_HEADER),
            HeaderValue::from_static(AUTH_VALUE),
        )],
        upstream: Url::parse(upstream).expect("valid upstream url"),
    }
}

/// Wall-clock seconds since the epoch — the warmth model and the interceptor fold
/// against the same clock, so the seeded cache must be timestamped near `now`.
fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

/// A warm cache on `MODEL` with a large positive prefix and a RECENT timestamp, so
/// the cache reads warm (not cold) and a flush at the candidate's offset clears the
/// NPV floor — the condition under which the controller chooses `Flush` over `Hold`.
fn warm_cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(8000),
        last_request_ts: now_s(),
        assumed_ttl_s: 3600.0,
        model: ModelId::new(MODEL),
        breakpoints: Vec::new(),
    }
}

/// A real `SessionEcon` for the loop: a known model (so `econ.econ` resolves and the
/// interceptor engages), a warm cache, a long horizon, and the captured summarizer
/// auth. No staged plan — `stage_next` produces it.
fn session_econ(upstream: &str) -> SessionEcon {
    let mut econ = SessionEcon::new(warm_cache(), auth(upstream), 0.0, PolicyConfig::default());
    econ.intercept_enabled = true;
    econ.remaining_turns = 50.0;
    econ
}

/// A tool-heavy request: an assistant `tool_use` plus a user `tool_result` carrying
/// a LARGE (>1024-char) file-read dump as real array content. Several later turns
/// push the pair out of the recency window so it is a squash candidate; a large
/// `system` keeps the POST-squash cacheable prefix above the model's floor, so the
/// flush's NPV stays positive on a warm cache.
fn tool_result_body() -> Bytes {
    let system = "you are a meticulous engineering assistant with deep context. ".repeat(90);
    let file_dump =
        "src/main.rs:42:    let config = Config::load(path).expect(\"config\"); ".repeat(40);
    assert!(
        file_dump.len() > 1024,
        "the tool_result payload must clear the dedup/span floor: {}",
        file_dump.len(),
    );
    Bytes::from(
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 4096,
            "system": system,
            "messages": [
                {"role": "user", "content": "read the main file and summarize it"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_read_1", "name": "Read",
                     "input": {"file_path": "src/main.rs"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_read_1", "content": file_dump}
                ]},
                {"role": "user", "content": "second turn"},
                {"role": "assistant", "content": "second reply"},
                {"role": "user", "content": "third turn"},
                {"role": "assistant", "content": "third reply"},
                {"role": "user", "content": "fourth turn that is current"},
            ],
        })
        .to_string()
        .into_bytes(),
    )
}

/// The content-address of the tool_result pair's whole-segment payload — the key the
/// organic `StagedPlan` must be keyed by. Computed the SAME way `stage_next` does
/// (segment the body, take the first `ToolPair`, address its payload bytes), so the
/// assertion proves the plan key without ever hand-building the plan.
fn tool_pair_ref(body: &[u8]) -> ccs_core::RefId {
    let parsed = parse_body(body).expect("parse");
    let segs = segment_prompt(&parsed);
    let seg = segs
        .iter()
        .find(|s| s.kind == SegmentKind::ToolPair)
        .expect("a ToolPair segment");
    content_address(&segment_payload_bytes(seg, &parsed))
}

/// Snapshot the L2 interceptor's inputs out of the session, CONSUMING the staged
/// plan (`take` — at most one apply per turn). This mirrors `relay::intercept_inputs`
/// exactly: it is the real L1 → L2 handoff, the plan came from `stage_next`.
fn intercept_inputs(econ: &Mutex<SessionEcon>, now: f64) -> InterceptInputs {
    let mut guard = econ.lock().expect("lock");
    let model_econ = guard.econ.expect("a known model resolves economics");
    InterceptInputs {
        econ: model_econ,
        cache: guard.cache.clone(),
        npv_floor: guard.npv_floor,
        policy: guard.policy,
        remaining_turns: guard.remaining_turns,
        hot_refs: guard.hot_refs.clone(),
        staged: guard.staged.take(),
        token_scale: guard.token_scale,
        now,
    }
}

#[tokio::test]
async fn closed_loop_stages_then_squashes_tool_result() {
    let summary = "condensed: main.rs loads config and runs the server";
    let upstream = mock_summarizer(summary).await;
    let store = test_store().await;
    let session = SessionId::new("tok-closed-loop");
    let body = tool_result_body();
    let pair_ref = tool_pair_ref(&body);

    // ── turn N: L1 stages the plan ORGANICALLY ─────────────────────────────────
    let econ = Arc::new(Mutex::new(session_econ(&upstream.uri())));
    stage_next(
        econ.clone(),
        body.clone(),
        session.clone(),
        store.clone(),
        now_s(),
    )
    .await;

    // The plan is produced by `stage_next`, never hand-seeded: non-empty, keyed by
    // the tool_result segment's content-address, carrying the summarizer's choice.
    {
        let guard = econ.lock().expect("lock");
        let plan = guard
            .staged
            .as_ref()
            .expect("stage_next populated `staged`");
        assert!(
            !plan.by_content.is_empty(),
            "a squashable tool_result with a non-Keep decision must stage an entry",
        );
        let entry = plan
            .by_content
            .get(&pair_ref)
            .expect("the plan is keyed by the tool_result's content-address ref_id");
        assert_eq!(
            entry.rec.kind,
            SegmentKind::ToolPair,
            "the staged entry is the tool_use/tool_result pair",
        );
        assert_eq!(
            entry.decision.choice,
            ChoiceTag::Compress,
            "a `summarize` decision with content self-normalizes to the reversible Compress",
        );
    }

    // The original was `put` under this session, so the ref the egress advertises
    // resolves back to the full file-read dump.
    assert!(
        matches!(
            store
                .retrieve(&pair_ref, &session, None, now_s())
                .await
                .expect("retrieve"),
            RetrieveResult::Hit { .. },
        ),
        "stage_next must have stored the original under the staged ref",
    );

    // ── turn N+1: L2 APPLIES that staged plan to the egress ─────────────────────
    let inputs = intercept_inputs(&econ, now_s());
    assert!(
        inputs.staged.is_some(),
        "the interceptor consumes the organic plan from the same SessionEcon",
    );
    assert!(
        inputs.hot_refs.is_empty(),
        "no ref is hot, so the RefHot pre-filter does not empty the batch",
    );
    let out = intercept::run(body.clone(), inputs).await;

    // The validity gate PASSED: a rejected rewrite fails open to identity, so a body
    // strictly smaller than the original proves the splice was accepted.
    assert!(
        out.bytes.len() < body.len(),
        "the egress body must SHRINK once the tool_result collapses to a placeholder ({} >= {})",
        out.bytes.len(),
        body.len(),
    );

    let egress: serde_json::Value =
        serde_json::from_slice(&out.bytes).expect("the spliced egress is valid JSON");
    let result_block = &egress["messages"][2]["content"][0];
    assert_eq!(
        result_block["type"], "tool_result",
        "the squashed block stays a tool_result so the TOOL_PAIR is never severed",
    );
    assert_eq!(
        result_block["tool_use_id"], "tu_read_1",
        "the tool_use_id is byte-intact so the assistant tool_use still pairs",
    );
    let content = result_block["content"]
        .as_str()
        .expect("the placeholder content is a string");
    assert!(
        content.contains(&format!("ref={}", pair_ref.as_str())),
        "the tool_result content is the placeholder carrying the staged ref marker",
    );
    assert!(
        content.contains(summary),
        "the placeholder carries the summarizer's summary line",
    );
    assert!(
        content.len() < 1024,
        "the placeholder is far smaller than the original dump it replaced",
    );

    // The assistant tool_use that opened the pair is untouched.
    assert_eq!(
        egress["messages"][1]["content"][0]["type"], "tool_use",
        "the assistant tool_use block survives the squash",
    );
    assert_eq!(
        egress["messages"][1]["content"][0]["id"], "tu_read_1",
        "the tool_use id is byte-intact",
    );

    // The ref the egress advertises still resolves to the original file-read dump —
    // the model could `retrieve` it verbatim.
    let resolved = store
        .retrieve(&pair_ref, &session, None, now_s())
        .await
        .expect("retrieve");
    let RetrieveResult::Hit { text, .. } = resolved else {
        panic!("the staged ref must still resolve after the squash");
    };
    assert!(
        text.contains("src/main.rs:42:"),
        "retrieve returns the original tool_result payload verbatim",
    );
    assert!(
        text.len() > 1024,
        "the resolved original is the full dump, not the placeholder",
    );

    // The only off-path traffic was the summarizer's call to the mocked boundary —
    // proof that L1 scored the segment via a real (mocked) decision, not a shortcut.
    let received = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert!(
        received
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(SUMMARIZER_MODEL)),
        "stage_next must have issued the off-path summarizer decision call",
    );
}

/// A second closed-loop turn proves the consumed plan does not re-apply: after the
/// interceptor `take`s the plan, the same `SessionEcon` has no staged plan, so the
/// next `intercept::run` forwards identity. Guards against a stale plan double-squash.
#[tokio::test]
async fn consumed_plan_does_not_resquash() {
    let upstream = mock_summarizer("condensed tool output").await;
    let store = test_store().await;
    let session = SessionId::new("tok-once");
    let body = tool_result_body();

    let econ = Arc::new(Mutex::new(session_econ(&upstream.uri())));
    stage_next(
        econ.clone(),
        body.clone(),
        session.clone(),
        store.clone(),
        now_s(),
    )
    .await;
    assert!(
        econ.lock().expect("lock").staged.is_some(),
        "stage_next populated the plan",
    );

    // First apply consumes the plan and squashes.
    let first = intercept::run(body.clone(), intercept_inputs(&econ, now_s())).await;
    assert!(first.bytes.len() < body.len(), "the first turn squashes");
    assert!(
        econ.lock().expect("lock").staged.is_none(),
        "intercept_inputs consumed (took) the staged plan",
    );

    // Second apply has no plan: identity forward (deterministic fallback is not
    // over-budget on this body, so the original passes through verbatim).
    let inputs = intercept_inputs(&econ, now_s());
    assert!(inputs.staged.is_none(), "no plan remains to apply");
    let second = intercept::run(body.clone(), inputs).await;
    assert_eq!(
        second.bytes, body,
        "with the plan consumed, the next turn forwards the ORIGINAL bytes unchanged",
    );
}
