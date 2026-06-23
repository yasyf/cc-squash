//! ccs-proxy binary entrypoint: parse spawn args, bind, serve until signalled.
//! The library crate (`ccs_proxy`) holds the relay itself.

use std::sync::Arc;

use ccs_proxy::seam::run_seam;
use ccs_proxy::{router, AppState};
use clap::Parser;
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Notify;

/// Command-line arguments for the supervised proxy child. The user-facing CLI is
/// the Go `ccs` binary; the proxy only parses its spawn args.
#[derive(Parser, Debug)]
#[command(name = "ccs-proxy", version)]
struct Args {
    /// Path to the Go control-plane seam socket (`proxy.sock`). When present the
    /// proxy connects, registers, and applies control frames; when absent it
    /// serves standalone (no-seam dev mode).
    #[arg(long)]
    socket: Option<String>,

    /// TCP port to listen on (127.0.0.1). 0 lets the OS assign a free port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    warn_if_unset("ENABLE_TOOL_SEARCH");
    warn_if_unset("DISABLE_AUTO_COMPACT");

    let args = Args::parse();

    // Bind directly and read back the assignment — no separate probe socket, so
    // no TOCTOU window between choosing a port and listening on it.
    let listener = TcpListener::bind(("127.0.0.1", args.port)).await?;
    let addr = listener.local_addr()?;

    // The Go control plane reads the chosen port from stderr to point Claude
    // Code at the relay.
    eprintln!("ccs-proxy listening on http://{addr}");
    tracing::info!(%addr, "ccs-proxy relay starting (Layer 1)");

    let state = AppState::new()?;

    // A seam `shutdown` frame and a SIGTERM/SIGINT both resolve through this one
    // notify, so the control plane can step the proxy down over the socket.
    let shutdown = Arc::new(Notify::new());

    // Connect the control-plane seam if a socket was given; fail open to
    // standalone if it is absent or the connect fails.
    if let Some(socket) = args.socket.as_deref() {
        match UnixStream::connect(socket).await {
            Ok(stream) => {
                tokio::spawn(run_seam(
                    stream,
                    state.clone(),
                    shutdown.clone(),
                    addr.port(),
                ));
            }
            Err(e) => {
                tracing::warn!(socket, error = %e, "seam connect failed; serving standalone");
            }
        }
    } else {
        tracing::info!("no --socket; serving standalone (no-seam dev mode)");
    }

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Resolve when to stop accepting connections: SIGTERM (sent by the Go
/// supervisor), SIGINT, or a seam `shutdown` frame routed through `seam`.
/// In-flight streams drain rather than cut mid-response.
async fn shutdown_signal(seam: Arc<Notify>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = seam.notified() => {},
    }
}

fn warn_if_unset(var: &str) {
    if std::env::var_os(var).is_none() {
        tracing::warn!(env = var, "expected environment variable is unset");
    }
}
