//! Verbatim upstream relay: send a buffered request to `api.anthropic.com` and
//! stream the response back byte-for-byte.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use axum::body::Body;
use axum::http::request::Parts;
use axum::response::Response;
use bytes::Bytes;

use crate::app::{bad_gateway, AppState};
use crate::headers::sanitize;

/// Forward the buffered request upstream. reqwest derives `Content-Length` from
/// these exact bytes, so the request framing cannot desync. A send failure
/// fails open to a 502.
pub async fn forward(state: &AppState, parts: Parts, body: Bytes) -> Response {
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
        Ok(upstream) => relay_response(upstream),
        Err(e) => {
            tracing::warn!(error = %e, "upstream send failed");
            bad_gateway()
        }
    }
}

fn relay_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = sanitize(upstream.headers());
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
