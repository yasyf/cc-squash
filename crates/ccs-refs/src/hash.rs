//! The sole content-hashing site. `content_address` is the only place in the
//! engine where bytes become a [`RefId`]; every other branding flows through
//! `ccs_core::RefId::parse`. The digest is sha256 (full 64-hex, never truncated)
//! so a ref is durable and cross-session — a truncated key could return a wrong
//! original. This same function is the §3d dedup key (one hash, both uses).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::RefId;
use sha2::{Digest, Sha256};

/// Content-address `bytes` into a `sha256:<64 lowercase hex>` [`RefId`].
///
/// This is the SOLE place raw bytes become a [`RefId`] — and the §3d dedup key.
///
/// Example:
///     >>> content_address(b"hello").as_str()
///     "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
pub fn content_address(bytes: &[u8]) -> RefId {
    match RefId::parse(&format!("sha256:{:x}", Sha256::digest(bytes))) {
        Ok(ref_id) => ref_id,
        // Unreachable: a sha256 lower-hex digest is exactly 64 chars of `0-9a-f`,
        // so `RefId::parse` of `sha256:<that>` can never fail.
        Err(_) => unreachable!("sha256 lower-hex digest is always a valid RefId"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sha256_vector() {
        assert_eq!(
            content_address(b"hello").as_str(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn empty_input_hashes() {
        assert_eq!(
            content_address(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn deterministic_and_distinct() {
        assert_eq!(content_address(b"abc"), content_address(b"abc"));
        assert_ne!(content_address(b"abc"), content_address(b"abd"));
    }
}
