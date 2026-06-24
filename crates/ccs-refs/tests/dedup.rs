//! §3d dedup gates and the backref render, exercised through the public API.

use ccs_refs::{can_dedupe_from, content_address, dedupe_key, render_backref, should_dedupe};

#[test]
fn dedupe_key_is_the_content_address() {
    let payload = b"a large repeated input that should dedupe by content";
    assert_eq!(dedupe_key(payload), content_address(payload));
}

#[test]
fn gates_skip_forced_short_and_assistant() {
    assert!(
        !should_dedupe("user", 4096, true, true),
        "forced is never deduped"
    );
    assert!(
        !should_dedupe("user", 1023, false, true),
        "below 1024 chars is skipped"
    );
    assert!(
        !should_dedupe("assistant", 4096, false, false),
        "assistant skipped unless allowed"
    );
    assert!(should_dedupe("assistant", 4096, false, true));
    assert!(should_dedupe("user", 1024, false, false));
}

#[test]
fn can_dedupe_from_same_role_or_assistant_to_user() {
    assert!(can_dedupe_from("user", "user"));
    assert!(can_dedupe_from("assistant", "assistant"));
    assert!(can_dedupe_from("assistant", "user"));
    assert!(!can_dedupe_from("user", "assistant"));
}

#[test]
fn backref_renders_and_is_re_extractable() {
    let id = content_address(b"the earlier message body");
    let backref = render_backref(&id);
    assert_eq!(
        backref,
        format!("[same as earlier message · ref={}]", id.as_str())
    );
    assert_eq!(ccs_refs::extract_refs(&backref), vec![id]);
}
