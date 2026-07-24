//! ccs-proxy binary entrypoint: parse spawn args, bind, serve until signalled.
//! The library crate (`ccs_proxy`) holds the relay itself.

use std::path::PathBuf;
use std::sync::Arc;

use ccs_proxy::seam::run_seam;
use ccs_proxy::{build_version::BUILD_VERSION, mcp_router, router, AppState};
use ccs_refs::RefStore;
use clap::Parser;
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Notify;

/// Command-line arguments for the supervised proxy child. The user-facing CLI is
/// the Go `ccs` binary; the proxy only parses its spawn args.
#[derive(Parser, Debug)]
#[command(name = "ccs-proxy", version = BUILD_VERSION)]
struct Args {
    /// Path to the Go control-plane epoch-1 seam socket. When present the
    /// proxy connects, registers, and applies control frames; when absent it
    /// serves standalone (no-seam dev mode).
    #[arg(long)]
    socket: Option<String>,

    /// TCP port to listen on (127.0.0.1). 0 lets the OS assign a free port.
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Exact path for the epoch-1 refs database. When absent the
    /// store opens at an ephemeral temp path (no-seam dev mode).
    #[arg(long)]
    refs_db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    warn_if_unset("ENABLE_TOOL_SEARCH");
    warn_if_unset("DISABLE_AUTO_COMPACT");

    // Bind directly and read back the assignment — no separate probe socket, so
    // no TOCTOU window between choosing a port and listening on it.
    let listener = TcpListener::bind(("127.0.0.1", args.port)).await?;
    let addr = listener.local_addr()?;

    // The SECOND listener — the rmcp `cc_squash_retrieve` MCP server — always on an
    // OS-assigned 127.0.0.1 port, isolated from the relay listener so a panic in the
    // MCP handler can never drop a relay connection.
    let mcp_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let mcp_addr = mcp_listener.local_addr()?;

    // The Go control plane reads the chosen port from stderr to point Claude
    // Code at the relay.
    eprintln!("ccs-proxy listening on http://{addr}");
    tracing::info!(%addr, %mcp_addr, "ccs-proxy relay starting (Layer 1)");

    let store = Arc::new(RefStore::open(refs_db_path(args.refs_db)).await?);
    let state = AppState::new(store)?;

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
                    mcp_addr.port(),
                ));
            }
            Err(e) => {
                tracing::warn!(socket, error = %e, "seam connect failed; serving standalone");
            }
        }
    } else {
        tracing::info!("no --socket; serving standalone (no-seam dev mode)");
    }

    // The MCP server runs as its own task on its own listener, isolated from the
    // relay: a panic or error here cannot drop a relay connection. It has no
    // graceful-shutdown of its own — when the relay's `serve` returns (signal or
    // seam `shutdown`), `main` returns and the runtime tears this task down. Drains
    // are a relay concern (minutes-long SSE); a retrieve is a sub-second round trip.
    let mcp_state = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(mcp_listener, mcp_router(mcp_state)).await;
    });

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

/// The exact refs database path under the seam, else an ephemeral epoch-1 path
/// keyed by pid for no-seam dev mode.
fn refs_db_path(path: Option<PathBuf>) -> PathBuf {
    match path {
        Some(path) => path,
        None => std::env::temp_dir().join(format!("ccs-refs-v1-{}.db", std::process::id())),
    }
}
