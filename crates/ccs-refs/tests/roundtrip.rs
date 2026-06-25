//! put → render_placeholder → extract_refs → materialize round-trips byte-identical;
//! idempotent puts; miss semantics.

use ccs_core::{MessageId, SegmentKind, SessionId};
use ccs_refs::{
    content_address, extract_refs, render_placeholder, RefStore, RetrieveResult, RECOVERY_HINT,
};
use tempfile::tempdir;

async fn store() -> (tempfile::TempDir, RefStore) {
    let dir = tempdir().unwrap();
    let store = RefStore::open(dir.path().join("refs.db")).await.unwrap();
    (dir, store)
}

#[tokio::test]
async fn put_render_extract_materialize_is_byte_identical() {
    let (_dir, store) = store().await;
    let original = b"the original tool output that was squashed away";
    let rec = store
        .put(
            original,
            &MessageId::new("uuid-1"),
            &SessionId::new("sess-1"),
            SegmentKind::ToolPair,
            100.0,
        )
        .await
        .unwrap();

    let placeholder = render_placeholder(&rec, "a short summary", false);
    let refs = extract_refs(&placeholder);
    assert_eq!(refs, vec![rec.ref_id.clone()]);

    let materialized = store.materialize(&refs[0], 200.0).await.unwrap().unwrap();
    assert_eq!(materialized.text.as_bytes(), original);
    assert_eq!(materialized.ref_id, rec.ref_id);
}

#[tokio::test]
async fn put_is_content_addressed_and_idempotent() {
    let (_dir, store) = store().await;
    let payload = b"identical payload stored twice";
    let a = store
        .put(
            payload,
            &MessageId::new("u-a"),
            &SessionId::new("s"),
            SegmentKind::Tools,
            1.0,
        )
        .await
        .unwrap();
    let b = store
        .put(
            payload,
            &MessageId::new("u-b"),
            &SessionId::new("s"),
            SegmentKind::Tools,
            2.0,
        )
        .await
        .unwrap();

    assert_eq!(a.ref_id, b.ref_id);
    assert_eq!(a.ref_id, content_address(payload));

    // Idempotent: the first write wins, the second is a no-op (one row).
    let m = store.materialize(&a.ref_id, 3.0).await.unwrap().unwrap();
    assert_eq!(m.text, "identical payload stored twice");
    // Exactly two materialize calls would happen; access_count proves a single row.
    assert_eq!(m.access_count, 1);
}

#[tokio::test]
async fn materialize_bumps_access_count() {
    let (_dir, store) = store().await;
    let rec = store
        .put(
            b"bumpable",
            &MessageId::new("u"),
            &SessionId::new("s"),
            SegmentKind::System,
            1.0,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .materialize(&rec.ref_id, 2.0)
            .await
            .unwrap()
            .unwrap()
            .access_count,
        1
    );
    assert_eq!(
        store
            .materialize(&rec.ref_id, 3.0)
            .await
            .unwrap()
            .unwrap()
            .access_count,
        2
    );
    assert_eq!(
        store
            .materialize(&rec.ref_id, 4.0)
            .await
            .unwrap()
            .unwrap()
            .access_count,
        3
    );
}

#[tokio::test]
async fn materialize_unknown_is_none() {
    let (_dir, store) = store().await;
    let unknown = content_address(b"never stored");
    assert!(store.materialize(&unknown, 1.0).await.unwrap().is_none());
}

#[tokio::test]
async fn retrieve_unknown_is_miss_not_panic() {
    let (_dir, store) = store().await;
    let unknown = content_address(b"missing");
    assert_eq!(
        store
            .retrieve(&unknown, &SessionId::new("s"), None, 1.0)
            .await
            .unwrap(),
        RetrieveResult::Miss
    );
    assert_eq!(
        store
            .retrieve(&unknown, &SessionId::new("s"), Some("q"), 1.0)
            .await
            .unwrap(),
        RetrieveResult::Miss
    );
    // The caller renders the recovery hint on a Miss.
    assert!(RECOVERY_HINT.contains("re-read the file"));
}

#[tokio::test]
async fn retrieve_without_query_returns_full_text() {
    let (_dir, store) = store().await;
    let original = "line one\nline two\nline three";
    let rec = store
        .put(
            original.as_bytes(),
            &MessageId::new("u"),
            &SessionId::new("s"),
            SegmentKind::UserTurn,
            1.0,
        )
        .await
        .unwrap();
    match store
        .retrieve(&rec.ref_id, &SessionId::new("s"), None, 2.0)
        .await
        .unwrap()
    {
        RetrieveResult::Hit { text, access_count } => {
            assert_eq!(text, original);
            assert_eq!(access_count, 1);
        }
        RetrieveResult::Miss => panic!("expected a hit"),
    }
}

#[tokio::test]
async fn retrieve_is_scoped_to_the_minting_session() {
    let (_dir, store) = store().await;
    let rec = store
        .put(
            b"secret owned by session a",
            &MessageId::new("u"),
            &SessionId::new("sess-a"),
            SegmentKind::Tools,
            1.0,
        )
        .await
        .unwrap();

    // A retrieve scoped to a different session is an indistinguishable Miss, and
    // it must not bump access_count.
    assert_eq!(
        store
            .retrieve(&rec.ref_id, &SessionId::new("sess-b"), None, 2.0)
            .await
            .unwrap(),
        RetrieveResult::Miss,
    );
    // The owning session still sees access_count == 1 (the cross-session attempt
    // never touched the row).
    match store
        .retrieve(&rec.ref_id, &SessionId::new("sess-a"), None, 3.0)
        .await
        .unwrap()
    {
        RetrieveResult::Hit { access_count, .. } => assert_eq!(access_count, 1),
        RetrieveResult::Miss => panic!("the owning session must hit"),
    }
}

#[tokio::test]
async fn fuse_flag_gates_the_read_line() {
    let (_dir, store) = store().await;
    let rec = store
        .put(
            b"x",
            &MessageId::new("u"),
            &SessionId::new("s"),
            SegmentKind::Tools,
            1.0,
        )
        .await
        .unwrap();
    assert!(!render_placeholder(&rec, "s", false).contains("Read("));
    assert!(render_placeholder(&rec, "s", true).contains("Read(\".cc-squash/refs/sha256-"));
}

#[tokio::test]
async fn store_reopens_across_sessions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("refs.db");
    let ref_id = {
        let store = RefStore::open(&path).await.unwrap();
        store
            .put(
                b"durable",
                &MessageId::new("u"),
                &SessionId::new("s"),
                SegmentKind::Tools,
                1.0,
            )
            .await
            .unwrap()
            .ref_id
    };
    let reopened = RefStore::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .materialize(&ref_id, 2.0)
            .await
            .unwrap()
            .unwrap()
            .text,
        "durable"
    );
}
