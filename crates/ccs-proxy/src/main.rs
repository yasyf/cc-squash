//! ccs-proxy — the cc-squash data plane.
//!
//! A streaming proxy at `ANTHROPIC_BASE_URL` that, on every `/v1/messages`
//! request, prices keep-vs-evict per context segment and rewrites the request
//! to minimise prompt-cache cost. Layer 1 is the RelayCore: a transparent,
//! fail-open passthrough plus the v0 `<summary>` synthesis capability. The
//! Go control plane (`ccs`) supervises this child over `proxy.sock`.
//!
//! Cardinal invariant: fail-open to identity. Any error/timeout/panic ⇒ forward
//! the original request and relay the original response byte-for-byte. Unlike the
//! rest of the repo, this crate does NOT "crash on the unexpected" on the hot
//! path — a relay that panics is a worse failure than a relay that passes
//! through unchanged (build plan §5/§9).

mod relay;
mod synth;

use std::net::TcpListener;

use clap::Parser;
use pingora::proxy::http_proxy_service;
use pingora::server::Server;

use crate::relay::CcsProxy;

/// Command-line arguments for the supervised proxy child. The user-facing CLI is
/// the Go `ccs` binary; the proxy only parses its spawn args.
#[derive(Parser, Debug)]
#[command(name = "ccs-proxy", version)]
struct Args {
    /// Path to the Go control-plane seam socket (`proxy.sock`). Accepted but
    /// unused in the Phase-0 spike.
    #[arg(long)]
    socket: Option<String>,

    /// TCP port to listen on (127.0.0.1). 0 lets the OS assign a free port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    warn_if_unset("ENABLE_TOOL_SEARCH");
    warn_if_unset("DISABLE_AUTO_COMPACT");

    let args = Args::parse();
    if let Some(socket) = args.socket.as_deref() {
        tracing::info!(socket, "control-plane seam socket (unused in spike)");
    }

    let port = resolve_port(args.port)?;
    let addr = format!("127.0.0.1:{port}");

    // The Go control plane reads the chosen port from stderr to point Claude
    // Code at the relay.
    eprintln!("ccs-proxy listening on http://{addr}");
    tracing::info!(%addr, "ccs-proxy relay starting (Layer 1 spike)");

    let mut server = Server::new(None)?;
    server.bootstrap();

    let mut proxy = http_proxy_service(&server.configuration, CcsProxy);
    proxy.add_tcp(&addr);
    server.add_service(proxy);

    server.run_forever();
}

/// Resolve the listen port: honour an explicit `--port`, or ask the OS for a
/// free one by binding ephemerally and reading back the assignment.
fn resolve_port(requested: u16) -> anyhow::Result<u16> {
    if requested != 0 {
        return Ok(requested);
    }
    let probe = TcpListener::bind("127.0.0.1:0")?;
    Ok(probe.local_addr()?.port())
}

fn warn_if_unset(var: &str) {
    if std::env::var_os(var).is_none() {
        tracing::warn!(env = var, "expected environment variable is unset");
    }
}
