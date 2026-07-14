//! The never-worse property (Phase 3 pass B, HARD GATE 2): for arbitrary JSON, the encoding
//! `select_leaner` chooses is never longer than `format-core`'s compact JSON — the crate's
//! implicit byte-net floor. A tabular encoding is only ever taken where it strictly shrinks,
//! so the keep-smaller rule can never enlarge a leaf. Plus a regression that a wide numeric
//! lexeme survives passes A and B byte-exact — the compact writer preserves it, and pass B
//! either keeps a lexeme-preserving encoding or the TOON number-safety guard rejects that
//! candidate.

use ccs_policy::pipeline::passes::json_minify::minify_json;
use ccs_policy::pipeline::passes::json_toon::select_leaner;
use proptest::prelude::*;

/// Number lexemes for the JSON generator: ordinary `i64`s plus lexemes that a serde_json
/// f64 round-trip would corrupt — 2^53 ± 1, wide integers and decimals, and exponents. The
/// compact writer must carry every one verbatim.
fn arb_number_lexeme() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<i64>().prop_map(|n| n.to_string()),
        Just("9007199254740991".to_string()), // 2^53 - 1
        Just("9007199254740992".to_string()), // 2^53
        Just("9007199254740993".to_string()), // 2^53 + 1 (f64 rounds it away)
        Just("12345678901234567890123456".to_string()), // 26-digit integer
        Just("3.14159265358979323846264338".to_string()), // 26-digit decimal
        Just("1.7976931348623157e308".to_string()),
        Just("-2.5e-3".to_string()),
        Just("1E6".to_string()),
    ]
}

/// A recursive arbitrary JSON *text* (not a serde_json `Value`, which would canonicalize
/// exotic number lexemes through f64 before the pass ever sees them): scalars at the leaves
/// — including the wide numeric lexemes above — objects/arrays nesting to a bounded depth.
/// The charset for strings and keys needs no JSON escaping, so the text is assembled by hand.
fn arb_json_text() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("null".to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        arb_number_lexeme(),
        "[a-zA-Z0-9 ._-]{0,32}".prop_map(|s| format!("\"{s}\"")),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..8)
                .prop_map(|items| format!("[{}]", items.join(","))),
            prop::collection::vec(("[a-z]{1,8}", inner), 0..8).prop_map(|pairs| format!(
                "{{{}}}",
                pairs
                    .iter()
                    .map(|(k, v)| format!("\"{k}\":{v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        ]
    })
}

proptest! {
    #[test]
    fn chosen_never_worse_than_compact_baseline(text in arb_json_text()) {
        let baseline = format_core::compact_json(&format_core::decode_ir(&text).expect("valid json text"));
        let chosen = select_leaner(&text).expect("valid json parses");
        prop_assert!(
            chosen.len() <= baseline.len(),
            "select_leaner enlarged past the compact baseline: chosen {} > baseline {}\n  text: {text}\n  chosen: {chosen}",
            chosen.len(),
            baseline.len(),
        );
    }
}

/// A wide numeric lexeme flows through pass A then pass B with its value byte-exact: pass A's
/// compact writer emits the lexeme verbatim, and pass B either keeps a lexeme-preserving
/// encoding or the TOON guard rejects the unsafe number (TOON is the only encoder that
/// routes a number through f64). The tabular `rows` case is the one where TOON would
/// otherwise win, so it exercises the guard's fallback.
#[test]
fn wide_numbers_survive_passes_a_and_b_byte_exact() {
    for lexeme in [
        "3.14159265358979323846264338", // 26-digit decimal
        "9007199254740993",             // 2^53 + 1
        "12345678901234567890123456",   // 26-digit integer
        "1.7976931348623157e308",
    ] {
        for src in [
            format!("{{\n  \"value\": {lexeme},\n  \"label\": \"measurement-of-a-thing\"\n}}"),
            format!(
                "{{\"rows\":[{{\"id\":1,\"v\":{lexeme}}},{{\"id\":2,\"v\":{lexeme}}},{{\"id\":3,\"v\":{lexeme}}}]}}"
            ),
        ] {
            let after_a = minify_json(&src).expect("valid json");
            assert!(
                after_a.contains(lexeme),
                "pass A mangled {lexeme}: {after_a}"
            );
            let after_b = select_leaner(&after_a).expect("valid json");
            assert!(
                after_b.contains(lexeme),
                "pass B mangled {lexeme}: {after_b}"
            );
        }
    }
}
