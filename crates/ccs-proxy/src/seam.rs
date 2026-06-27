//! The Rust client end of the `proxy.sock` seam. The Go control plane binds and
//! listens on `proxy.sock`; this proxy is spawned with `--socket=<path>` and
//! connects as the client. After connecting it sends a single [`Register`] frame
//! announcing its bound TCP port, version, and pid; thereafter the control plane
//! streams [`Control`] frames (mint/evict/shadow/kill/shutdown) one line-delimited
//! JSON object at a time.
//!
//! The read loop is the sole writer to the control surface on [`AppState`]:
//! `sessions`, `kill`, `shadow`, and `config`. Everything is fail-open — an
//! absent socket, a connect failure, a dropped stream, or a malformed line is
//! logged and the relay keeps serving standalone. The seam dropping is never
//! fatal, so this module carries no panics on its path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use ccs_core::RefId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use crate::app::AppState;
use crate::config::RelayConfig;
use crate::demux::{SessionCtx, SessionToken};

/// The grace window a ref is protected from GC after it is first stored. MUST
/// exceed the worst-case squash→persist latency: a ref that was just staged and
/// spliced but whose owning session hasn't yet re-staged (so it's no longer in
/// `reachable`) must not be evicted out from under an in-flight turn. Generous on
/// purpose — refs are cheap, a wrongly-evicted live ref is a broken retrieve.
const GRACE_SECONDS: f64 = 600.0;

/// The non-reachable live byte budget GC drives toward, oldest-accessed first.
/// 256 MiB: large enough that a normal session never sheds a still-useful ref,
/// small enough that an abandoned db doesn't grow without bound.
const MAX_REFS_BYTES: u64 = 256 * 1024 * 1024;

/// The Rust -> Go announcement, sent once right after the proxy binds its TCP
/// ports. The Go side reads `port`/`mcp_port`/`pid` as `int`, so a `u16`/`u32`
/// serialises to a JSON number it accepts unchanged. `mcp_port` is the SECOND
/// listener — the rmcp `cc_squash_retrieve` MCP server — distinct from the relay
/// `port`.
#[derive(Debug, Serialize)]
struct Register {
    #[serde(rename = "type")]
    kind: &'static str,
    port: u16,
    mcp_port: u16,
    version: &'static str,
    pid: u32,
}

impl Register {
    fn announce(port: u16, mcp_port: u16) -> Self {
        Self {
            kind: "register",
            port,
            mcp_port,
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
        }
    }
}

/// A Go -> Rust control frame. Internally tagged on `type`; unknown fields are
/// ignored (the per-session config slice deserialises permissively), and the
/// `config` payload defaults to an empty [`RelayConfig`] when Go omits it.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Mint {
        token: String,
        #[serde(default)]
        config: RelayConfig,
    },
    Evict {
        token: String,
    },
    Shadow {
        on: bool,
    },
    Kill {
        on: bool,
    },
    Gc,
    Shutdown,
}

/// Connect, register, and run the read loop until the stream ends or a
/// `shutdown` frame arrives. Generic over the transport so a test can drive it
/// with a `tokio` `UnixStream` pair. A write failure on the register frame, a
/// malformed line, or a closed stream all fail open: log and return, leaving the
/// relay serving standalone.
pub async fn run_seam<S>(
    stream: S,
    state: AppState,
    shutdown: Arc<Notify>,
    port: u16,
    mcp_port: u16,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    match serde_json::to_vec(&Register::announce(port, mcp_port)) {
        Ok(mut frame) => {
            frame.push(b'\n');
            if let Err(e) = write_half.write_all(&frame).await {
                tracing::warn!(error = %e, "seam register write failed; serving standalone");
                return;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "seam register encode failed; serving standalone");
            return;
        }
    }
    tracing::info!(port, "seam registered with control plane");

    let mut lines = BufReader::new(read_half).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match apply(&line, &state) {
                ControlOutcome::Continue => {}
                ControlOutcome::Shutdown => {
                    tracing::info!("seam received shutdown");
                    shutdown.notify_one();
                    return;
                }
            },
            Ok(None) => {
                tracing::warn!("seam closed by control plane; serving standalone");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "seam read failed; serving standalone");
                return;
            }
        }
    }
}

/// The effect of one control frame on the loop.
enum ControlOutcome {
    Continue,
    Shutdown,
}

/// Parse one line and apply it to the control surface. A malformed line is logged
/// and skipped (fail-open), keeping the loop alive for the next frame.
fn apply(line: &str, state: &AppState) -> ControlOutcome {
    match serde_json::from_str::<Control>(line) {
        Ok(Control::Mint { token, config }) => {
            state.sessions.insert(
                SessionToken(token.clone()),
                SessionCtx {
                    config,
                    session_id: ccs_core::SessionId::new(token),
                    econ: None,
                },
            );
            ControlOutcome::Continue
        }
        Ok(Control::Evict { token }) => {
            state.sessions.remove(&SessionToken(token));
            ControlOutcome::Continue
        }
        Ok(Control::Shadow { on }) => {
            state.shadow.store(on, Ordering::Relaxed);
            ControlOutcome::Continue
        }
        Ok(Control::Kill { on }) => {
            state.kill.store(on, Ordering::Relaxed);
            ControlOutcome::Continue
        }
        Ok(Control::Gc) => {
            run_gc(state);
            ControlOutcome::Continue
        }
        Ok(Control::Shutdown) => ControlOutcome::Shutdown,
        Err(e) => {
            tracing::warn!(error = %e, "seam dropping malformed frame");
            ControlOutcome::Continue
        }
    }
}

/// Compute the GC reachable set synchronously (a brief lock per session) and spawn
/// the eviction off the seam loop. Reachable = the union of every session's STAGED
/// ref ids — the plans about to be spliced onto the next turn — so a ref that is
/// staged-but-not-yet-applied is never evicted. LRU + grace protect refs that were
/// recently retrieved or stored; this set covers the staged-but-cold gap.
fn run_gc(state: &AppState) {
    let reachable = reachable_refs(state);
    let store = state.store.clone();
    tokio::spawn(async move {
        match store
            .gc(&reachable, GRACE_SECONDS, MAX_REFS_BYTES, now_s())
            .await
        {
            Ok(evicted) => tracing::info!(evicted, reachable = reachable.len(), "seam gc swept"),
            Err(e) => tracing::warn!(error = %e, "seam gc failed"),
        }
    });
}

/// The union of every session's staged ref ids — the keys of each
/// `SessionEcon.staged.by_content` across the `sessions` DashMap. Each session's
/// `econ` lock is taken only for the brief synchronous extent of the clone, never
/// across an `.await`; a poisoned lock or an uninitialised `econ` contributes
/// nothing (the worst case is one extra eligible-but-cold ref, not a wrong evict
/// of a live one — the grace window backstops it).
fn reachable_refs(state: &AppState) -> HashSet<RefId> {
    state
        .sessions
        .iter()
        .filter_map(|entry| entry.econ.clone())
        .filter_map(|econ| {
            let guard = econ.lock().ok()?;
            Some(
                guard
                    .staged
                    .as_ref()
                    .map(|plan| plan.by_content.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
        })
        .flatten()
        .collect()
}

fn now_s() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::LazyLock;

    use ccs_refs::RefStore;
    use tempfile::TempDir;

    use super::*;
    use crate::session::SessionEcon;

    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn test_db_path() -> PathBuf {
        TEST_DIR.path().join(format!(
            "refs-{}.db",
            DB_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_store() -> Arc<RefStore> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        Arc::new(rt.block_on(RefStore::open(test_db_path())).unwrap())
    }

    fn state() -> AppState {
        AppState::with_upstream(
            reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
            test_store(),
        )
        .unwrap()
    }

    #[test]
    fn register_frame_shape() {
        let json = serde_json::to_value(Register::announce(8080, 9090)).unwrap();
        assert_eq!(json["type"], "register");
        assert_eq!(json["port"], 8080);
        assert_eq!(json["mcp_port"], 9090);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["pid"], std::process::id());
    }

    #[test]
    fn mint_inserts_session_with_config() {
        let state = state();
        assert!(matches!(
            apply(r#"{"type":"mint","token":"tok-a","config":{}}"#, &state),
            ControlOutcome::Continue
        ));
        assert!(state
            .sessions
            .contains_key(&SessionToken("tok-a".to_owned())));
    }

    #[test]
    fn mint_without_config_defaults() {
        let state = state();
        apply(r#"{"type":"mint","token":"tok-b"}"#, &state);
        assert!(state
            .sessions
            .contains_key(&SessionToken("tok-b".to_owned())));
    }

    #[test]
    fn mint_ignores_unknown_config_fields() {
        let state = state();
        apply(
            r#"{"type":"mint","token":"tok-c","config":{"future_knob":42}}"#,
            &state,
        );
        assert!(state
            .sessions
            .contains_key(&SessionToken("tok-c".to_owned())));
    }

    #[test]
    fn evict_removes_session() {
        let state = state();
        apply(r#"{"type":"mint","token":"tok-d","config":{}}"#, &state);
        apply(r#"{"type":"evict","token":"tok-d"}"#, &state);
        assert!(!state
            .sessions
            .contains_key(&SessionToken("tok-d".to_owned())));
    }

    #[test]
    fn kill_and_shadow_flip_flags() {
        let state = state();
        apply(r#"{"type":"kill","on":true}"#, &state);
        assert!(state.kill.load(Ordering::Relaxed));
        apply(r#"{"type":"shadow","on":true}"#, &state);
        assert!(state.shadow.load(Ordering::Relaxed));
        apply(r#"{"type":"kill","on":false}"#, &state);
        assert!(!state.kill.load(Ordering::Relaxed));
    }

    #[test]
    fn shutdown_frame_signals_shutdown() {
        let state = state();
        assert!(matches!(
            apply(r#"{"type":"shutdown"}"#, &state),
            ControlOutcome::Shutdown
        ));
    }

    #[test]
    fn malformed_frame_is_skipped_not_fatal() {
        let state = state();
        assert!(matches!(
            apply("{ not json", &state),
            ControlOutcome::Continue
        ));
        assert!(matches!(
            apply(r#"{"type":"unknown-kind","x":1}"#, &state),
            ControlOutcome::Continue
        ));
    }

    #[test]
    fn gc_frame_is_continue_and_reachable_is_the_staged_union() {
        use std::collections::HashMap;

        use ccs_core::{MessageId, SegmentKind, SessionId};
        use ccs_policy::ContentDecision;
        use ccs_refs::RefRecord;

        use crate::staging::{StagedEntry, StagedPlan};

        let state = state();
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Stage one ref under a session (reachable), and store an OLD ref that no
        // session references (unreachable). Both go through the real store.
        let staged_id = rt
            .block_on(state.store.put(
                &vec![b's'; 5000],
                &MessageId::new("u"),
                &SessionId::new("sess-x"),
                SegmentKind::Tools,
                1000.0,
            ))
            .unwrap()
            .ref_id;
        let old_id = rt
            .block_on(state.store.put(
                &vec![b'o'; 5000],
                &MessageId::new("u"),
                &SessionId::new("sess-x"),
                SegmentKind::Tools,
                0.0,
            ))
            .unwrap()
            .ref_id;

        let mut econ = SessionEcon::new(
            test_cache(),
            test_auth(),
            0.0,
            ccs_policy::PolicyConfig::default(),
        );
        econ.staged = Some(StagedPlan {
            by_content: HashMap::from([(
                staged_id.clone(),
                StagedEntry {
                    rec: RefRecord {
                        ref_id: staged_id.clone(),
                        byte_len: 5000,
                        token_estimate: ccs_core::TokenCount(1000),
                        source_uuid: MessageId::new("u"),
                        session_id: SessionId::new("sess-x"),
                        kind: SegmentKind::Tools,
                        created_at: 1000.0,
                    },
                    decision: ContentDecision {
                        choice: ccs_core::ChoiceTag::Compress,
                        ranges_to_keep: Vec::new(),
                        summary_content: None,
                    },
                    recode: None,
                },
            )]),
        });
        state.sessions.insert(
            SessionToken("sess-x".to_owned()),
            SessionCtx {
                config: RelayConfig::default(),
                session_id: SessionId::new("sess-x"),
                econ: Some(Arc::new(std::sync::Mutex::new(econ))),
            },
        );

        let reachable = reachable_refs(&state);
        assert_eq!(reachable.len(), 1);
        assert!(reachable.contains(&staged_id));
        assert!(!reachable.contains(&old_id));

        // Drive the eviction with that reachable set: aggressive params (every
        // unreachable ref over cap, far past grace) so only the staged-protection
        // can keep `staged_id` alive. The staged ref survives; the old one evicts.
        let evicted = rt
            .block_on(
                state
                    .store
                    .gc(&reachable, GRACE_SECONDS, 0, 1000.0 + GRACE_SECONDS + 1.0),
            )
            .unwrap();
        assert_eq!(evicted, 1, "exactly the unreachable old ref is evicted");
        assert!(
            rt.block_on(state.store.materialize(&staged_id, 9999.0))
                .unwrap()
                .is_some(),
            "the staged (reachable) ref must survive GC",
        );
        assert!(
            rt.block_on(state.store.materialize(&old_id, 9999.0))
                .unwrap()
                .is_none(),
            "the unreachable old ref must be evicted",
        );

        // The Gc control frame itself is non-fatal (spawns the sweep, keeps the loop).
        let _guard = rt.enter();
        assert!(matches!(
            apply(r#"{"type":"gc"}"#, &state),
            ControlOutcome::Continue
        ));
    }

    fn test_cache() -> ccs_economics::CacheState {
        ccs_economics::CacheState {
            cached_prefix_tokens: ccs_core::TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: ccs_core::ModelId::new("claude-opus-4-8"),
            breakpoints: Vec::new(),
        }
    }

    fn test_auth() -> ccs_summarizer::SessionAuthContext {
        ccs_summarizer::SessionAuthContext {
            headers: Vec::new(),
            upstream: reqwest::Url::parse("https://api.anthropic.com").unwrap(),
        }
    }
}
