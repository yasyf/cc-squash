//! Verbatim upstream relay: send a buffered request to `api.anthropic.com` and
//! stream the response back byte-for-byte. The inspected path may tap the
//! response stream (read-only) to fold cache usage into the session; the tap
//! never alters the relayed bytes.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use axum::body::Body;
use axum::http::request::Parts;
use axum::response::Response;
use bytes::Bytes;

use crate::app::{bad_gateway, AppState};
use crate::headers::sanitize;
use crate::usage_tap::{self, UsageSink};

/// Forward the buffered request upstream. reqwest derives `Content-Length` from
/// these exact bytes, so the request framing cannot desync. A send failure
/// fails open to a 502. `sink`, when present, taps the response stream
/// (read-only) for the first `message_start` cache usage; the relayed bytes are
/// identical with or without it.
pub async fn forward(
    state: &AppState,
    parts: Parts,
    body: Bytes,
    sink: Option<UsageSink>,
) -> Response {
    let mut url = state.upstream.clone();
    url.set_path(parts.uri.path());
    url.set_query(parts.uri.query());

    match state
        .client
        .request(parts.method, url)
        .headers(sanitize(&parts.headers))
        .body(body)
        .send()
        .await
    {
        Ok(upstream) => relay_response(upstream, sink),
        Err(e) => {
            tracing::warn!(error = %e, "upstream send failed");
            bad_gateway()
        }
    }
}

fn relay_response(upstream: reqwest::Response, sink: Option<UsageSink>) -> Response {
    let status = upstream.status();
    let headers = sanitize(upstream.headers());
    let body = match sink {
        Some(sink) => Body::from_stream(usage_tap::tap(upstream.bytes_stream(), sink)),
        None => Body::from_stream(upstream.bytes_stream()),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
