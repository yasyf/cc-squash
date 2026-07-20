//! GC correctness: never delete a reachable ref; within-grace survives; eligible
//! over-cap rows evict LRU-oldest-first.

use std::collections::HashSet;

use ccs_core::{MessageId, RefId, SegmentKind, SessionId};
use ccs_refs::RefStore;
use tempfile::tempdir;

async fn store() -> (tempfile::TempDir, RefStore) {
    let dir = tempdir().unwrap();
    let store = RefStore::open(dir.path().join("refs-v1.db")).await.unwrap();
    (dir, store)
}

async fn put(store: &RefStore, payload: &[u8], created_at: f64) -> RefId {
    store
        .put(
            payload,
            &MessageId::new("u"),
            &SessionId::new("s"),
            SegmentKind::Tools,
            created_at,
        )
        .await
        .unwrap()
        .ref_id
}

fn reachable(ids: &[&RefId]) -> HashSet<RefId> {
    ids.iter().map(|r| (*r).clone()).collect()
}

#[tokio::test]
async fn reachable_ref_is_never_deleted_even_over_cap_and_grace() {
    let (_dir, store) = store().await;
    let id = put(&store, &vec![b'a'; 10_000], 0.0).await;

    // now=1000 (far past grace), max_bytes=0 (everything over cap), but the ref
    // is reachable — it MUST survive.
    let deleted = store.gc(&reachable(&[&id]), 1.0, 0, 1000.0).await.unwrap();
    assert_eq!(deleted, 0);
    assert!(store.materialize(&id, 2000.0).await.unwrap().is_some());
}

#[tokio::test]
async fn within_grace_unreachable_survives() {
    let (_dir, store) = store().await;
    let id = put(&store, &vec![b'b'; 10_000], 100.0).await;

    // now - created_at = 5 <= grace 60: not yet eligible, even unreachable + over cap.
    let deleted = store.gc(&HashSet::new(), 60.0, 0, 105.0).await.unwrap();
    assert_eq!(deleted, 0);
    assert!(store.materialize(&id, 200.0).await.unwrap().is_some());
}

#[tokio::test]
async fn unreachable_unpinned_past_grace_over_cap_is_evicted() {
    let (_dir, store) = store().await;
    let id = put(&store, &vec![b'c'; 10_000], 0.0).await;

    let deleted = store.gc(&HashSet::new(), 60.0, 0, 1000.0).await.unwrap();
    assert_eq!(deleted, 1);
    assert!(store.materialize(&id, 2000.0).await.unwrap().is_none());
}

#[tokio::test]
async fn under_cap_eligible_rows_survive() {
    let (_dir, store) = store().await;
    let id = put(&store, &[b'd'; 100], 0.0).await;

    // Eligible (past grace, unreachable) but live total (100) <= max_bytes (1MB).
    let deleted = store
        .gc(&HashSet::new(), 60.0, 1_000_000, 1000.0)
        .await
        .unwrap();
    assert_eq!(deleted, 0);
    assert!(store.materialize(&id, 2000.0).await.unwrap().is_some());
}

#[tokio::test]
async fn lru_evicts_oldest_access_first_until_under_cap() {
    let (_dir, store) = store().await;
    // Three 1000-byte refs, all eligible. Cap = 1500 → must drop bytes until <= 1500,
    // i.e. evict 2 of 3 (3000 → 2000 → 1000). Oldest last_access first.
    let old = put(&store, &vec![b'1'; 1000], 0.0).await;
    let mid = put(&store, &vec![b'2'; 1000], 0.0).await;
    let new = put(&store, &vec![b'3'; 1000], 0.0).await;

    // Touch them with increasing `now` so last_access_at orders old < mid < new.
    store.materialize(&old, 10.0).await.unwrap();
    store.materialize(&mid, 20.0).await.unwrap();
    store.materialize(&new, 30.0).await.unwrap();

    let deleted = store.gc(&HashSet::new(), 60.0, 1500, 1000.0).await.unwrap();
    assert_eq!(deleted, 2);
    // The two oldest-accessed are gone; the most-recently-accessed survives.
    assert!(store.materialize(&old, 2000.0).await.unwrap().is_none());
    assert!(store.materialize(&mid, 2000.0).await.unwrap().is_none());
    assert!(store.materialize(&new, 2000.0).await.unwrap().is_some());
}

#[tokio::test]
async fn reachable_subset_protected_while_rest_evicted() {
    let (_dir, store) = store().await;
    let keep = put(&store, &vec![b'k'; 5000], 0.0).await;
    let drop = put(&store, &vec![b'x'; 5000], 0.0).await;

    // keep is reachable (e.g. still in the in-flight stream); drop is not.
    let deleted = store
        .gc(&reachable(&[&keep]), 60.0, 0, 1000.0)
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(store.materialize(&keep, 2000.0).await.unwrap().is_some());
    assert!(store.materialize(&drop, 2000.0).await.unwrap().is_none());
}
