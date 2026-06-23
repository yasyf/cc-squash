//! Per-session demux: the `/s/{token}/…` surface the Go control plane points a
//! supervised Claude Code session at. A registered token's `POST /v1/messages`
//! is inspected (synthesize-or-forward); every other path/method, and any
//! unknown, expired, or malformed token, fails open to a verbatim passthrough.
//! NEVER a 404 — a stale `$(ccs url)` reused after the session ended must still
//! reach upstream unchanged.
//!
//! axum 0.7's matchit router can't host a named-param route (`/s/{token}/…`)
//! alongside a catch-all (`/s/*rest`) sharing the prefix, so a single catch-all
//! handler owns the whole `/s/` subtree and dispatches internally. The forwarded
//! upstream path is the INNER path: `/s/{token}/v1/messages` becomes
//! `/v1/messages` before [`crate::forward::forward`] (which forwards the request
//! path and query verbatim) ever sees it.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use axum::extract::{Path, Request, State};
use axum::http::uri::{PathAndQuery, Uri};
use axum::http::Method;
use axum::response::Response;

use crate::app::AppState;
use crate::config::RelayConfig;
use crate::relay::{serve, Inspect};

/// The inner endpoint a registered session's request is inspected on.
const MESSAGES_PATH: &str = "/v1/messages";

/// A Claude Code session token, the first path segment in `/s/{token}/…`.
/// Branded so a raw `String` can never be mistaken for one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionToken(pub String);

/// Per-session relay state the control plane registers when it spawns a session.
/// Layer 1 holds only the session's [`RelayConfig`], which defaults to the
/// global config.
#[derive(Debug, Clone, Default)]
pub struct SessionCtx {
    pub config: RelayConfig,
}

/// `/s/*rest` for any method: the entire per-session surface. `rest` is
/// `{token}/{inner…}`; a registered token's `POST /v1/messages` is inspected,
/// everything else is a verbatim passthrough. The `/s/{token}` prefix is
/// stripped before forwarding so upstream sees the inner path.
pub async fn session(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    req: Request,
) -> Response {
    let (token, inner) = match rest.split_once('/') {
        Some((token, inner)) => (token, format!("/{inner}")),
        None => (rest.as_str(), "/".to_owned()),
    };
    let inspect = inspect_for(&state, &method, &inner, token);
    serve(state, strip_prefix(req, token), inspect).await
}

/// Inspect only a registered session's `POST /v1/messages`. An unknown/expired/
/// absent token, a non-POST method, or any other inner path fails open to
/// passthrough. Never 404.
fn inspect_for(state: &AppState, method: &Method, inner_path: &str, token: &str) -> Inspect {
    match (
        method == Method::POST,
        inner_path == MESSAGES_PATH,
        state.sessions.contains_key(&SessionToken(token.to_owned())),
    ) {
        (true, true, true) => Inspect::Yes,
        _ => Inspect::No,
    }
}

/// Rewrite the request URI to the inner path by dropping the leading
/// `/s/{token}` segment, preserving the query verbatim. A reconstruction that
/// cannot be expressed as a valid URI leaves the request untouched (fail-open).
fn strip_prefix(mut req: Request, token: &str) -> Request {
    let prefix = format!("/s/{token}");
    let pq = req.uri().path_and_query().map(PathAndQuery::as_str);
    if let Some(inner) = pq.and_then(|pq| inner_path_and_query(pq, &prefix)) {
        if let Ok(uri) = inner.parse::<Uri>() {
            *req.uri_mut() = uri;
        }
    }
    req
}

fn inner_path_and_query(path_and_query: &str, prefix: &str) -> Option<String> {
    let rest = path_and_query.strip_prefix(prefix)?;
    Some(match rest {
        "" => "/".to_owned(),
        _ if rest.starts_with('?') => format!("/{rest}"),
        _ => rest.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn rewritten(uri: &str, token: &str) -> String {
        let req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        strip_prefix(req, token)
            .uri()
            .path_and_query()
            .expect("path and query")
            .to_string()
    }

    #[test]
    fn strips_prefix_from_messages_path() {
        assert_eq!(rewritten("/s/tok123/v1/messages", "tok123"), "/v1/messages");
    }

    #[test]
    fn strips_prefix_preserving_query() {
        assert_eq!(
            rewritten("/s/tok123/v1/messages?beta=true", "tok123"),
            "/v1/messages?beta=true",
        );
    }

    #[test]
    fn strips_prefix_from_nested_path() {
        assert_eq!(
            rewritten("/s/tok123/v1/models/list", "tok123"),
            "/v1/models/list",
        );
    }

    #[test]
    fn bare_prefix_becomes_root() {
        assert_eq!(rewritten("/s/tok123", "tok123"), "/");
    }

    #[test]
    fn query_only_after_prefix_becomes_root_query() {
        assert_eq!(rewritten("/s/tok123?x=1", "tok123"), "/?x=1");
    }
}
