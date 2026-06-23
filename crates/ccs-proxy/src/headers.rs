//! Header hygiene for the proxy hop.

use http::HeaderMap;

/// Connection-specific headers that must not cross a proxy hop (RFC 7230 §6.1),
/// plus the framing/routing headers the new stack re-derives itself: `host`
/// (reqwest sets it from the upstream URL) and `content-length` (reqwest sets it
/// from the request body; hyper sets it from the relayed response stream).
const STRIP: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
    "content-length",
];

/// Copy `headers`, dropping the hop-by-hop and framing entries, ready to attach
/// to the opposite side of the hop. Preserves duplicate values for repeated
/// header names.
pub fn sanitize(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !STRIP.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(
                HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        h
    }

    #[test]
    fn strips_hop_by_hop_and_framing() {
        let out = sanitize(&map(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("te", "trailers"),
            ("trailer", "x-checksum"),
            ("upgrade", "h2c"),
            ("proxy-connection", "keep-alive"),
            ("host", "localhost:1234"),
            ("content-length", "42"),
        ]));
        assert!(out.is_empty());
    }

    #[test]
    fn keeps_end_to_end_headers() {
        let out = sanitize(&map(&[
            ("authorization", "Bearer x"),
            ("x-api-key", "sk-ant-123"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "messages-2024"),
            ("content-type", "application/json"),
            ("accept-encoding", "gzip"),
        ]));
        assert_eq!(out.get("authorization").unwrap(), "Bearer x");
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-123");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(out.get("anthropic-beta").unwrap(), "messages-2024");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("accept-encoding").unwrap(), "gzip");
    }

    #[test]
    fn is_case_insensitive() {
        let out = sanitize(&map(&[
            ("Transfer-Encoding", "chunked"),
            ("Content-Type", "text/plain"),
        ]));
        assert!(!out.contains_key("transfer-encoding"));
        assert_eq!(out.get("content-type").unwrap(), "text/plain");
    }

    #[test]
    fn preserves_duplicate_values() {
        let out = sanitize(&map(&[("x-multi", "a"), ("x-multi", "b")]));
        let values: Vec<&str> = out
            .get_all("x-multi")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, vec!["a", "b"]);
    }
}
