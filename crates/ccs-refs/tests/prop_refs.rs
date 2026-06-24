//! Property tests: content_address is deterministic and collision-free on
//! distinct inputs, and GC never deletes a ref in `reachable` for any random
//! ref set + reachable subset + grace/cap.

use std::collections::HashSet;

use ccs_core::{MessageId, RefId, SegmentKind, SessionId};
use ccs_refs::{content_address, RefStore};
use proptest::prelude::*;
use tempfile::tempdir;

proptest! {
    #[test]
    fn content_address_is_deterministic(bytes: Vec<u8>) {
        prop_assert_eq!(content_address(&bytes), content_address(&bytes));
    }

    #[test]
    fn distinct_inputs_address_distinctly(a: Vec<u8>, b: Vec<u8>) {
        prop_assume!(a != b);
        prop_assert_ne!(content_address(&a), content_address(&b));
    }

    #[test]
    fn gc_never_deletes_a_reachable_ref(
        payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..64), 1..12),
        reachable_mask in prop::collection::vec(any::<bool>(), 12),
        grace in 0.0f64..50.0,
        max_bytes in 0u64..256,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let dir = tempdir().unwrap();
            let store = RefStore::open(dir.path().join("refs.db")).await.unwrap();

            let mut ids: Vec<RefId> = Vec::new();
            for payload in &payloads {
                let id = store
                    .put(payload, &MessageId::new("u"), &SessionId::new("s"), SegmentKind::Tools, 0.0)
                    .await
                    .unwrap()
                    .ref_id;
                ids.push(id);
            }

            let reachable: HashSet<RefId> = ids
                .iter()
                .enumerate()
                .filter(|(i, _)| reachable_mask.get(*i).copied().unwrap_or(false))
                .map(|(_, id)| id.clone())
                .collect();

            // now well past grace so eligibility is maximally aggressive.
            store.gc(&reachable, grace, max_bytes, 1000.0).await.unwrap();

            for id in &reachable {
                prop_assert!(
                    store.materialize(id, 1000.0).await.unwrap().is_some(),
                    "a reachable ref was deleted by gc"
                );
            }
            Ok(())
        })?;
    }
}
