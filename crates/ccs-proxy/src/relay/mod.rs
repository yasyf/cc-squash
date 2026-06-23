//! The RelayCore: a transparent, fail-open `ProxyHttp` over `api.anthropic.com`.
//!
//! Every request is forwarded upstream byte-for-byte except recognised
//! compaction requests, which the relay answers locally with a synthesized
//! `<summary>` SSE stream (see [`crate::synth`]).
//!
//! # Why the body is handled in `request_body_filter`
//!
//! Detection needs the full request body, but draining it in `request_filter`
//! poisons pingora's forwarding loop: after a drain `is_body_done()` is true, so
//! the H1 proxy streams an empty body upstream
//! (`pingora-proxy-0.8.1/src/proxy_h1.rs:305-420`). The retry buffer is no
//! escape either — it caps at 64 KiB (`BODY_BUF_LIMIT`), truncating the large
//! `/v1/messages` bodies this relay sees. So the relay instead accumulates the
//! body in `request_body_filter` (the one callback pingora feeds every chunk
//! before forwarding, `proxy_h1.rs:783`), suppressing output until end of stream:
//!
//! - **Forward path:** the full buffer is re-emitted as the final body chunk, so
//!   the upstream receives the exact bytes the client sent, at any size.
//! - **Synth path:** the synthesized SSE response is written to the client and a
//!   sentinel error aborts upstream proxying; [`CcsProxy::fail_to_proxy`]
//!   recognises the sentinel and suppresses the default error response.
//!
//! Cardinal invariant: fail-open to identity. Detection or synthesis errors fall
//! through to a verbatim upstream forward; the relay never emits a partial stream
//! and never panics on the hot path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};

use crate::synth;

const UPSTREAM_HOST: &str = "api.anthropic.com";
const UPSTREAM_ADDR: (&str, u16) = (UPSTREAM_HOST, 443);

/// Sentinel error type raised after a synthesized response is written, so the
/// proxy aborts upstream forwarding without emitting its own error body.
const SYNTH_SHORT_CIRCUIT: ErrorType = ErrorType::Custom("ccs-synth-short-circuit");

/// Per-request state shared across the proxy filters.
#[derive(Default)]
pub struct RequestCtx {
    /// Accumulated request body bytes, populated chunk-by-chunk in
    /// `request_body_filter` before the forward-or-synth decision.
    body: BytesMut,
    /// Set once a synthesized response has been written to the client.
    synthesized: bool,
}

/// The transparent, fail-open relay over the Anthropic Messages API.
pub struct CcsProxy;

#[async_trait]
impl ProxyHttp for CcsProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx::default()
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(
            UPSTREAM_ADDR,
            true,
            UPSTREAM_HOST.to_string(),
        )))
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(chunk) = body.take() {
            ctx.body.extend_from_slice(&chunk);
        }

        if !end_of_stream {
            // Hold the body back until the whole request has arrived; emitting
            // nothing here is explicitly tolerated by the forward loop
            // (proxy_h1.rs:794-796).
            return Ok(());
        }

        match synth::detect(&ctx.body) {
            Some(inputs) => match write_synthesized(session, &inputs).await {
                Ok(()) => {
                    ctx.synthesized = true;
                    Err(Error::new(SYNTH_SHORT_CIRCUIT))
                }
                // Fail-open: if writing the synthesized stream fails we cannot
                // also forward (output may be partially written), so surface the
                // write error rather than corrupt the relay contract.
                Err(e) => Err(e),
            },
            None => {
                *body = Some(ctx.body.split().freeze());
                Ok(())
            }
        }
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request.insert_header("Host", UPSTREAM_HOST)?;
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>> {
        // Verbatim SSE passthrough: never touch upstream response bytes.
        Ok(None)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy
    where
        Self::CTX: Send + Sync,
    {
        if ctx.synthesized {
            // The synthesized response is already on the wire; suppress the
            // default error response (error_code 0 writes nothing downstream).
            return FailToProxy {
                error_code: 0,
                can_reuse_downstream: false,
            };
        }
        self.default_fail_to_proxy(session, e).await
    }
}

impl CcsProxy {
    async fn default_fail_to_proxy(&self, session: &mut Session, e: &Error) -> FailToProxy {
        let code = error_status(e);
        if code > 0 {
            if let Err(write_err) = session.respond_error(code).await {
                tracing::error!(error = %write_err, "failed to send error response downstream");
            }
        }
        FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }
}

fn error_status(e: &Error) -> u16 {
    use pingora::ErrorSource::*;
    use pingora::ErrorType::*;
    match e.etype() {
        HTTPStatus(code) => *code,
        _ => match e.esource() {
            Upstream => 502,
            Downstream => match e.etype() {
                WriteError | ReadError | ConnectionClosed => 0,
                _ => 400,
            },
            Internal | Unset => 500,
        },
    }
}

async fn write_synthesized(session: &mut Session, inputs: &synth::BriefInputs) -> Result<()> {
    let mut header = ResponseHeader::build(200, Some(2))?;
    header.insert_header("content-type", "text/event-stream")?;
    header.insert_header("cache-control", "no-cache")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    let events = synth::synth_events(inputs);
    let last = events.len().saturating_sub(1);
    for (i, event) in events.into_iter().enumerate() {
        session.write_response_body(Some(event), i == last).await?;
    }
    Ok(())
}
