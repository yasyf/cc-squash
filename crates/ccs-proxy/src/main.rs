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

use clap::Parser;

/// Command-line arguments for the supervised proxy child. The user-facing CLI is
/// the Go `ccs` binary; the proxy only parses its spawn args.
#[derive(Parser, Debug)]
#[command(name = "ccs-proxy", version)]
struct Args {
    /// Path to the Go control-plane seam socket (`proxy.sock`).
    #[arg(long)]
    socket: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    tracing::info!(socket = %args.socket, "ccs-proxy starting (Layer 1 skeleton)");
    Ok(())
}
