//! Cal-floor: the per-model `min_cache_floor` values read straight from the
//! economics table, plus the sub-floor / at-floor comparison semantics economics
//! exposes (the actual breakpoint guard lives in policy; here we only pin the
//! values and the inclusive at-floor comparison).

use ccs_core::{ModelId, TokenCount};
use ccs_economics::economics_for;

fn floor(model: &str) -> TokenCount {
    economics_for(&ModelId::new(model)).unwrap().min_cache_floor
}

#[test]
fn floors_match_the_table() {
    let cases = [
        ("claude-opus-4-8", TokenCount(1024)),
        ("claude-sonnet-4-6", TokenCount(1024)),
        ("claude-sonnet-4-5", TokenCount(1024)),
        ("claude-haiku-4-5-20251001", TokenCount(4096)),
        ("claude-haiku-4-5", TokenCount(4096)),
    ];
    for (model, expected) in cases {
        assert_eq!(floor(model), expected);
    }
}

#[test]
fn unknown_model_has_no_economics() {
    assert_eq!(economics_for(&ModelId::new("gpt-4")), None);
    assert_eq!(economics_for(&ModelId::new("claude-opus-4-7")), None);
}

#[test]
fn sub_floor_prefix_is_below_opus_floor() {
    let floor = floor("claude-opus-4-8"); // 1024
                                          // 900 < 1024 ⇒ sub-floor
    assert!(TokenCount(900) < floor);
    // 1024 is at-floor: it clears the inclusive `>=` guard (so it is not sub-floor).
    assert!(TokenCount(1024) >= floor);
    // 1025 clears it strictly.
    assert!(TokenCount(1025) > floor);
}

#[test]
fn haiku_floor_is_four_times_opus() {
    let opus = floor("claude-opus-4-8");
    let haiku = floor("claude-haiku-4-5");
    assert_eq!(haiku.get(), opus.get() * 4);
    // A 2048-token prefix is fine for Opus but sub-floor for Haiku.
    assert!(TokenCount(2048) >= opus);
    assert!(TokenCount(2048) < haiku);
}
