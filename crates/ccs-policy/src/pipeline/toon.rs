//! The TOON encoder shim. Wraps `toon-format`'s `encode` behind the one option set
//! the JSON→TOON pass (Phase 3 pass B) uses: tab delimiter, key-folding off (the
//! "expandPaths OFF" / encode-only configuration). `toon-format` is pure (it depends
//! only on `serde`/`serde_json`), so it stays inside the `ccs-policy` purity boundary.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde_json::Value;
use toon_format::{Delimiter, EncodeOptions};

/// The fixed encode configuration for the JSON→TOON pass: tab delimiter, key-folding
/// off. A single source of truth so the pass and its keep-smaller test agree.
pub fn toon_options() -> EncodeOptions {
    EncodeOptions::new().with_delimiter(Delimiter::Tab)
}

/// Encode a JSON value to TOON under [`toon_options`]. `None` when the value cannot be
/// encoded (the pass then keeps the minified JSON), so a caller never has to reason
/// about `toon-format`'s error type.
pub fn toon_encode(value: &Value, opts: &EncodeOptions) -> Option<String> {
    toon_format::encode(value, opts).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn uniform_array_shrinks_vs_minified() {
        let value = json!({
            "rows": [
                {"id": 1, "name": "a"},
                {"id": 2, "name": "b"},
                {"id": 3, "name": "c"},
            ]
        });
        let toon = toon_encode(&value, &toon_options()).expect("encodes");
        let minified = serde_json::to_string(&value).expect("serializes");
        assert!(
            toon.len() < minified.len(),
            "TOON ({}) should shrink a uniform array vs minified JSON ({})",
            toon.len(),
            minified.len(),
        );
        assert!(toon.contains('\t'), "tab delimiter is used");
    }

    #[test]
    fn primitive_encodes() {
        assert!(toon_encode(&json!(42), &toon_options()).is_some());
    }
}
