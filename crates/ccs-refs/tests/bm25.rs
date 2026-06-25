//! BM25 search-within, both directly and through `RefStore::retrieve(query)`.

use ccs_core::{MessageId, SegmentKind, SessionId};
use ccs_refs::bm25::search_within;
use ccs_refs::{RefStore, RetrieveResult};
use tempfile::tempdir;

const DOC: &str = "Setup: install the dependencies and run the migration.\n\
    The authentication module validates the bearer token on every request.\n\
    Pagination uses an opaque cursor encoded in base64.\n\
    Rate limiting is enforced per API key with a sliding window.\n\
    The metrics endpoint exposes prometheus counters for each route.\n\
    Database connections are pooled with a maximum of sixteen.";

#[test]
fn query_selects_the_relevant_passage() {
    let result = search_within(DOC, "authentication bearer token");
    assert!(result.contains("authentication module validates the bearer token"));
}

#[test]
fn empty_query_returns_leading_passages() {
    let result = search_within(DOC, "");
    assert!(result.starts_with("Setup: install the dependencies"));
}

#[test]
fn no_match_returns_a_sensible_default() {
    let result = search_within(DOC, "kubernetes helm istio");
    assert!(!result.is_empty());
    assert!(result.starts_with("Setup: install the dependencies"));
}

#[tokio::test]
async fn retrieve_with_query_searches_within_the_stored_original() {
    let dir = tempdir().unwrap();
    let store = RefStore::open(dir.path().join("refs.db")).await.unwrap();
    let rec = store
        .put(
            DOC.as_bytes(),
            &MessageId::new("u"),
            &SessionId::new("s"),
            SegmentKind::Tools,
            1.0,
        )
        .await
        .unwrap();

    match store
        .retrieve(
            &rec.ref_id,
            &SessionId::new("s"),
            Some("rate limiting sliding window"),
            2.0,
        )
        .await
        .unwrap()
    {
        RetrieveResult::Hit { text, .. } => {
            assert!(text.contains("Rate limiting is enforced"));
        }
        RetrieveResult::Miss => panic!("expected a hit"),
    }
}
