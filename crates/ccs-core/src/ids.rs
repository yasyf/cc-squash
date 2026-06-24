//! Branded `String` identifiers shared across the engine: model, ref, and message
//! ids. `RefId` is parse-only — it can be validated from its `sha256:` wire form
//! but never fabricated from raw content; minting a `RefId` from bytes is
//! `ccs-refs`' sole responsibility in Layer 3.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;

use serde::{Deserialize, Serialize};

const REF_PREFIX: &str = "sha256:";
const REF_HEX_LEN: usize = 64;

/// An Anthropic model identifier, e.g. `claude-opus-4-8`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(String);

/// A conversation message identifier (a Claude Code message UUID).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(String);

/// A content-addressed reference: `sha256:` followed by exactly 64 lowercase hex
/// characters. Parse-only — there is deliberately no constructor that hashes raw
/// content, so a `RefId` can only ever name a digest `ccs-refs` already minted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RefId(String);

/// Why a string failed to parse as a [`RefId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefIdError {
    /// Missing the required `sha256:` prefix.
    MissingPrefix,
    /// The digest was not exactly 64 characters long.
    BadLength,
    /// The digest contained a character outside lowercase hex (`0-9a-f`).
    NonHex,
}

impl ModelId {
    /// Brand a raw model string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The underlying model string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MessageId {
    /// Brand a raw message id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The underlying message-id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl RefId {
    /// Parse and validate a `sha256:<64 lowercase hex>` reference.
    pub fn parse(s: &str) -> Result<Self, RefIdError> {
        let digest = s
            .strip_prefix(REF_PREFIX)
            .ok_or(RefIdError::MissingPrefix)?;
        match digest.len() {
            REF_HEX_LEN if digest.bytes().all(is_lower_hex) => Ok(Self(s.to_owned())),
            REF_HEX_LEN => Err(RefIdError::NonHex),
            _ => Err(RefIdError::BadLength),
        }
    }

    /// The full `sha256:…` reference string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for RefIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingPrefix => "ref id must start with `sha256:`",
            Self::BadLength => "ref id digest must be exactly 64 hex characters",
            Self::NonHex => "ref id digest must be lowercase hex",
        })
    }
}

impl std::error::Error for RefIdError {}

impl<'de> Deserialize<'de> for RefId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let raw = String::deserialize(deserializer)?;
        RefId::parse(&raw).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    #[test]
    fn ref_id_parses_valid_sha256() {
        let s = format!("sha256:{HEX64}");
        assert_eq!(RefId::parse(&s).unwrap().as_str(), s);
    }

    #[test]
    fn ref_id_rejects_missing_prefix() {
        assert_eq!(RefId::parse(HEX64), Err(RefIdError::MissingPrefix));
        assert_eq!(
            RefId::parse(&format!("md5:{HEX64}")),
            Err(RefIdError::MissingPrefix)
        );
    }

    #[test]
    fn ref_id_rejects_wrong_length() {
        assert_eq!(RefId::parse("sha256:abc"), Err(RefIdError::BadLength));
        assert_eq!(
            RefId::parse(&format!("sha256:{HEX64}f")),
            Err(RefIdError::BadLength)
        );
        assert_eq!(
            RefId::parse(&format!("sha256:{}", &HEX64[..63])),
            Err(RefIdError::BadLength)
        );
    }

    #[test]
    fn ref_id_rejects_uppercase_and_non_hex() {
        let upper = HEX64.to_uppercase();
        assert_eq!(
            RefId::parse(&format!("sha256:{upper}")),
            Err(RefIdError::NonHex)
        );
        let with_g = format!("sha256:g{}", &HEX64[1..]);
        assert_eq!(RefId::parse(&with_g), Err(RefIdError::NonHex));
    }

    #[test]
    fn ref_id_deserialize_validates() {
        let ok = format!("\"sha256:{HEX64}\"");
        assert_eq!(
            serde_json::from_str::<RefId>(&ok).unwrap().as_str(),
            &ok[1..ok.len() - 1]
        );
        assert!(serde_json::from_str::<RefId>("\"not-a-ref\"").is_err());
        assert!(
            serde_json::from_str::<RefId>(&format!("\"sha256:{}\"", HEX64.to_uppercase())).is_err()
        );
    }

    #[test]
    fn ref_id_serialize_roundtrips_string() {
        let s = format!("sha256:{HEX64}");
        let id = RefId::parse(&s).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{s}\""));
    }

    #[test]
    fn model_and_message_ids_brand_strings() {
        assert_eq!(ModelId::new("claude-opus-4-8").as_str(), "claude-opus-4-8");
        assert_eq!(MessageId::new("uuid-1").as_str(), "uuid-1");
    }
}
