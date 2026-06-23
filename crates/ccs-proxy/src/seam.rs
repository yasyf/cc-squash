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

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use crate::app::AppState;
use crate::config::RelayConfig;
use crate::demux::{SessionCtx, SessionToken};

/// The Rust -> Go announcement, sent once right after the proxy binds its TCP
/// port. The Go side reads `port`/`pid` as `int`, so a `u16`/`u32` serialises to
/// a JSON number it accepts unchanged.
#[derive(Debug, Serialize)]
struct Register {
    #[serde(rename = "type")]
    kind: &'static str,
    port: u16,
    version: &'static str,
    pid: u32,
}

impl Register {
    fn announce(port: u16) -> Self {
        Self {
            kind: "register",
            port,
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
    Shutdown,
}

/// Connect, register, and run the read loop until the stream ends or a
/// `shutdown` frame arrives. Generic over the transport so a test can drive it
/// with a `tokio` `UnixStream` pair. A write failure on the register frame, a
/// malformed line, or a closed stream all fail open: log and return, leaving the
/// relay serving standalone.
pub async fn run_seam<S>(stream: S, state: AppState, shutdown: Arc<Notify>, port: u16)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    match serde_json::to_vec(&Register::announce(port)) {
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
            state
                .sessions
                .insert(SessionToken(token), SessionCtx { config });
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
        Ok(Control::Shutdown) => ControlOutcome::Shutdown,
        Err(e) => {
            tracing::warn!(error = %e, "seam dropping malformed frame");
            ControlOutcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::with_upstream(reqwest::Url::parse("http://127.0.0.1:1").unwrap()).unwrap()
    }

    #[test]
    fn register_frame_shape() {
        let json = serde_json::to_value(Register::announce(8080)).unwrap();
        assert_eq!(json["type"], "register");
        assert_eq!(json["port"], 8080);
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
}
