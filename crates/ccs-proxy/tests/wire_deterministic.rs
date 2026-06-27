//! The Phase 3 WIRE closed-loop proof: L1 `stage_next` runs the deterministic recode
//! chain (F→D→E→A→B→C→J) OFF-PATH, stages a `Recode` (inline-lossless or ref-backed), then
//! L2 `intercept::run` APPLIES it on the next turn through the unchanged Controller, splice,
//! and validate seam — no hand-seeded plan. The only mock is the Anthropic boundary, and the
//! summarizer is told to `keep`: a fired deterministic recode PREEMPTS the LLM, so the
//! rewrite is purely deterministic. Proves a JSON tool_result shrinks to a ref-backed TOON
//! and `retrieve` returns the byte-exact original, an ANSI-laden log shrinks to an
//! inline-lossless cleaned form with NO ref marker, and every rewrite still passes the
//! validity gate (shrink-only, tool-pair intact).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use ccs_core::{ModelId, SegmentKind, SessionId, TokenCount};
use ccs_economics::CacheState;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_payload_bytes, segment_prompt, PolicyConfig};
use ccs_proxy::intercept::{self, InterceptInputs};
use ccs_proxy::session::SessionEcon;
use ccs_proxy::staging::stage_next;
use ccs_refs::{content_address, RefStore, RetrieveResult};
use ccs_summarizer::SessionAuthContext;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "claude-opus-4-8";
const AUTH_HEADER: &str = "x-api-key";
const AUTH_VALUE: &str = "sk-wire-det";

async fn test_store() -> Arc<RefStore> {
    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().expect("temp dir"));
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);
    let path = TEST_DIR.path().join(format!(
        "refs-{}.db",
        DB_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    Arc::new(RefStore::open(path).await.expect("open refs db"))
}

/// A mock Anthropic upstream that answers EVERY `/v1/messages` with a `keep` decision: the
/// deterministic recode preempts the LLM, so the summarizer's choice never reaches the plan.
async fn mock_keep() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_sum",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": r#"{"choice":"keep"}"#}],
        })))
        .mount(&upstream)
        .await;
    upstream
}

fn auth(upstream: &str) -> SessionAuthContext {
    SessionAuthContext {
        headers: vec![(
            HeaderName::from_static(AUTH_HEADER),
            HeaderValue::from_static(AUTH_VALUE),
        )],
        upstream: Url::parse(upstream).expect("valid upstream url"),
    }
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

fn warm_cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(8000),
        last_request_ts: now_s(),
        assumed_ttl_s: 3600.0,
        model: ModelId::new(MODEL),
        breakpoints: Vec::new(),
    }
}

fn session_econ(upstream: &str) -> SessionEcon {
    let mut econ = SessionEcon::new(warm_cache(), auth(upstream), 0.0, PolicyConfig::default());
    econ.intercept_enabled = true;
    econ.remaining_turns = 50.0;
    econ
}

/// A tool-heavy body whose historical tool_result carries `dump`. A large `system` keeps the
/// post-squash cacheable prefix above the model floor so the warm flush clears the NPV gate;
/// later turns push the pair out of the recency window so it is a squash candidate.
fn body_with_tool_result(dump: &str) -> Bytes {
    let system = "you are a meticulous engineering assistant with deep context. ".repeat(90);
    Bytes::from(
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 4096,
            "system": system,
            "messages": [
                {"role": "user", "content": "read the data and summarize it"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_read_1", "name": "Read",
                     "input": {"file_path": "data"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_read_1", "content": dump}
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

fn tool_pair_ref(body: &[u8]) -> ccs_core::RefId {
    let parsed = parse_body(body).expect("parse");
    let segs = segment_prompt(&parsed);
    let seg = segs
        .iter()
        .find(|s| s.kind == SegmentKind::ToolPair)
        .expect("a ToolPair segment");
    content_address(&segment_payload_bytes(seg, &parsed))
}

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

/// Stage the body and return the resulting egress + the squashed tool_result block.
async fn stage_then_intercept(
    upstream: &MockServer,
    store: &Arc<RefStore>,
    session: &SessionId,
    body: &Bytes,
) -> (Bytes, serde_json::Value) {
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
        "stage_next must populate a plan for a deterministically-recodeable body",
    );
    let inputs = intercept_inputs(&econ, now_s());
    let out = intercept::run(body.clone(), inputs).await;
    let egress: serde_json::Value =
        serde_json::from_slice(&out.bytes).expect("the spliced egress is valid JSON");
    let block = egress["messages"][2]["content"][0].clone();
    (out.bytes, block)
}

/// A uniform-array JSON dump — the TOON sweet spot: the ref-backed B pass strictly shrinks it
/// to tab-delimited TOON.
fn uniform_json_dump() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "rows": (0..60)
            .map(|i| serde_json::json!({"id": i, "name": "alpha-beta-gamma", "ok": true}))
            .collect::<Vec<_>>(),
    }))
    .expect("json")
}

#[tokio::test]
async fn json_tool_result_recodes_to_toon_ref_backed_and_round_trips() {
    let upstream = mock_keep().await;
    let store = test_store().await;
    let session = SessionId::new("tok-toon");
    let dump = uniform_json_dump();
    let body = body_with_tool_result(&dump);
    let pair_ref = tool_pair_ref(&body);

    let (egress_bytes, block) = stage_then_intercept(&upstream, &store, &session, &body).await;

    // The rewrite shrank the body (the validity gate would fail open on a non-shrink).
    assert!(
        egress_bytes.len() < body.len(),
        "the deterministic TOON recode must shrink the egress body",
    );
    // The tool_pair is intact: a tool_result block keeping its tool_use_id.
    assert_eq!(block["type"], "tool_result", "stays a tool_result");
    assert_eq!(block["tool_use_id"], "tu_read_1", "tool_use_id survives");
    let content = block["content"].as_str().expect("string content");
    // Ref-backed: the cleaned TOON body PLUS the resolved ref marker for retrieve.
    assert!(
        content.contains('\t'),
        "the recoded body is tab-delimited TOON"
    );
    assert!(
        content.contains(&format!("ref={}", pair_ref.as_str())),
        "a ref-backed recode bakes the ref= marker for retrieve",
    );
    assert!(
        content.len() < dump.len(),
        "the recoded content is smaller than the original JSON dump",
    );

    // The stored original round-trips BYTE-EXACT: `retrieve` returns the full pretty JSON.
    let resolved = store
        .retrieve(&pair_ref, &session, None, now_s())
        .await
        .expect("retrieve");
    let RetrieveResult::Hit { text, .. } = resolved else {
        panic!("the ref-backed recode must store a resolvable original");
    };
    let original_payload = {
        let parsed = parse_body(&body).expect("parse");
        let segs = segment_prompt(&parsed);
        let seg = segs
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .expect("tool pair");
        String::from_utf8(segment_payload_bytes(seg, &parsed)).expect("utf8")
    };
    assert_eq!(
        text, original_payload,
        "retrieve returns the byte-exact original tool_result payload",
    );
    assert!(
        text.contains("alpha-beta-gamma"),
        "the resolved original is the full JSON dump, not the TOON",
    );

    // The egress content being tab-TOON (asserted above) is itself the proof of preemption:
    // an LLM `summarize`/`compress` would have rendered the prose summary placeholder, never
    // tab-delimited TOON. The deterministic chain produced the plan.
}

#[tokio::test]
async fn ansi_tool_result_recodes_inline_lossless_no_ref_marker() {
    let upstream = mock_keep().await;
    let store = test_store().await;
    let session = SessionId::new("tok-ansi");
    // An ANSI-laden, non-JSON log: the inline-lossless D pass strips the escapes/CRs. The
    // chain shrinks it well past the 20% preempt floor, so it stages inline (no ref). Kept
    // under the head/tail line threshold (HEAD_LINES + TAIL_LINES = 60) so pass J never
    // converts it to a ref-backed truncate — the recode stays purely inline-lossless.
    let dump =
        "\x1b[2K\rbuilding \x1b[33m[####    ]\x1b[0m step \x1b[31mFAILED\x1b[0m here\n".repeat(50);
    let body = body_with_tool_result(&dump);
    let pair_ref = tool_pair_ref(&body);

    let (egress_bytes, block) = stage_then_intercept(&upstream, &store, &session, &body).await;

    assert!(
        egress_bytes.len() < body.len(),
        "the inline ANSI-strip recode must shrink the egress body",
    );
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "tu_read_1");
    let content = block["content"].as_str().expect("string content");
    // Inline-lossless: the model reads the CLEANED text directly — no escapes, no ref marker.
    assert!(!content.contains('\x1b'), "ANSI escapes are stripped");
    assert!(!content.contains('\r'), "carriage returns are stripped");
    assert!(
        !content.contains(&format!("ref={}", pair_ref.as_str())),
        "an inline-lossless recode carries NO ref marker (nothing to retrieve)",
    );
    assert!(
        content.contains("building") && content.contains("FAILED"),
        "the cleaned text preserves the real log content",
    );
}

#[tokio::test]
async fn consumed_recode_plan_does_not_resquash() {
    // The recode plan is single-apply: after the interceptor `take`s it, the next turn has no
    // plan and forwards identity — the deterministic recode never double-applies.
    let upstream = mock_keep().await;
    let store = test_store().await;
    let session = SessionId::new("tok-once-recode");
    let body = body_with_tool_result(&uniform_json_dump());

    let econ = Arc::new(Mutex::new(session_econ(&upstream.uri())));
    stage_next(
        econ.clone(),
        body.clone(),
        session.clone(),
        store.clone(),
        now_s(),
    )
    .await;
    let first = intercept::run(body.clone(), intercept_inputs(&econ, now_s())).await;
    assert!(
        first.bytes.len() < body.len(),
        "the first turn recodes + shrinks"
    );
    assert!(
        econ.lock().expect("lock").staged.is_none(),
        "the plan was consumed",
    );

    let second = intercept::run(body.clone(), intercept_inputs(&econ, now_s())).await;
    assert_eq!(
        second.bytes, body,
        "with the plan consumed, the next turn forwards the original verbatim",
    );
}
