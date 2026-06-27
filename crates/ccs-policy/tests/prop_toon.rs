//! The TOON never-worse property (Phase 3 pass B, HARD GATE 2): for arbitrary JSON, the
//! encoding `encode_smaller` chooses is never longer than minified JSON. TOON is only ever
//! taken where it strictly shrinks, so the keep-smaller rule can never enlarge a leaf.

use ccs_policy::pipeline::passes::json_toon::encode_smaller;
use proptest::prelude::*;
use serde_json::Value;

/// A recursive arbitrary JSON value: scalars at the leaves, objects/arrays nesting up to a
/// bounded depth — the spread the keep-smaller rule must never enlarge.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        "[a-zA-Z0-9 ,._-]{0,32}".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..8).prop_map(Value::from),
            prop::collection::hash_map("[a-z]{1,8}", inner, 0..8)
                .prop_map(|m| Value::from(serde_json::Map::from_iter(m))),
        ]
    })
}

proptest! {
    #[test]
    fn toon_is_never_worse_than_minified(value in arb_json()) {
        let minified = serde_json::to_string(&value).unwrap();
        let chosen = encode_smaller(&minified).expect("minified JSON re-parses");
        prop_assert!(
            chosen.len() <= minified.len(),
            "encode_smaller enlarged a leaf: chosen {} > minified {}\n  value: {minified}\n  chosen: {chosen}",
            chosen.len(),
            minified.len(),
        );
    }
}
