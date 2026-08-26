//! Content-addressed artifact identifiers and redaction metadata.
//!
//! Wire form for [`ArtifactId`] is `sha256:` followed by 64 lowercase hex
//! digits. [`ArtifactRef`] is metadata only: raw secret bytes are never a
//! field and are rejected if supplied on the wire.

use std::error::Error;
use std::fmt;
use std::str::{self, FromStr};

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Artifact-ref schema version (not a wire field).
pub const ARTIFACT_REF_SCHEMA: u16 = 1;

/// Canonical content-address prefix.
pub const ARTIFACT_ID_PREFIX: &str = "sha256:";

/// SHA-256 digest length in bytes.
pub const ARTIFACT_DIGEST_LEN: usize = 32;

/// Lowercase hex length of a SHA-256 digest.
pub const ARTIFACT_ID_HEX_LEN: usize = ARTIFACT_DIGEST_LEN * 2;

const ARTIFACT_ID_WIRE_LEN: usize = ARTIFACT_ID_PREFIX.len() + ARTIFACT_ID_HEX_LEN;
const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";
const ARTIFACT_REF_FIELDS: &[&str] = &["id", "media_type", "bytes", "redaction"];

/// SHA-256 content digest. Invalid wire forms cannot be constructed.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ArtifactId([u8; ARTIFACT_DIGEST_LEN]);

/// Parse failure for a non-canonical artifact identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactIdParseError;

/// Content-addressed blob reference. Metadata only; no payload bytes.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub media_type: String,
    pub bytes: u64,
    pub redaction: RedactionClass,
}

/// Sensitivity class of artifact content. Variants carry no payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RedactionClass {
    Public,
    Project,
    Sensitive,
    Secret,
}

/// Parse failure for an unknown redaction class string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedactionClassParseError;

impl ArtifactId {
    /// Hash `bytes` with SHA-256 and return the canonical content address.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut id = [0u8; ARTIFACT_DIGEST_LEN];
        id.copy_from_slice(&digest);
        Self(id)
    }

    /// Digest bytes that address this artifact.
    pub const fn as_digest(&self) -> &[u8; ARTIFACT_DIGEST_LEN] {
        &self.0
    }

    fn encode_wire(self) -> [u8; ARTIFACT_ID_WIRE_LEN] {
        let mut out = [0u8; ARTIFACT_ID_WIRE_LEN];
        out[..ARTIFACT_ID_PREFIX.len()].copy_from_slice(ARTIFACT_ID_PREFIX.as_bytes());
        for (i, byte) in self.0.iter().copied().enumerate() {
            let at = ARTIFACT_ID_PREFIX.len() + i * 2;
            out[at] = HEX_TABLE[(byte >> 4) as usize];
            out[at + 1] = HEX_TABLE[(byte & 0x0f) as usize];
        }
        out
    }

    fn wire_str(self) -> String {
        let buf = self.encode_wire();
        // Prefix and hex alphabet are ASCII, so the buffer is always valid UTF-8.
        str::from_utf8(&buf)
            .expect("artifact id wire form is ASCII")
            .to_owned()
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buf = self.encode_wire();
        // Prefix and hex alphabet are ASCII, so the buffer is always valid UTF-8.
        f.write_str(str::from_utf8(&buf).expect("artifact id wire form is ASCII"))
    }
}

impl fmt::Debug for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ArtifactId")
            .field(&self.to_string())
            .finish()
    }
}

impl FromStr for ArtifactId {
    type Err = ArtifactIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_canonical_artifact_id(s)
    }
}

impl Serialize for ArtifactId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(ArtifactIdVisitor)
    }
}

struct ArtifactIdVisitor;

impl Visitor<'_> for ArtifactIdVisitor {
    type Value = ArtifactId;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a sha256:<64 lowercase hex> artifact identifier")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

impl fmt::Display for ArtifactIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed artifact identifier")
    }
}

impl Error for ArtifactIdParseError {}

impl ArtifactRef {
    pub fn new(
        id: ArtifactId,
        media_type: impl Into<String>,
        bytes: u64,
        redaction: RedactionClass,
    ) -> Self {
        Self {
            id,
            media_type: media_type.into(),
            bytes,
            redaction,
        }
    }
}

impl Serialize for ArtifactRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ArtifactRef", 4)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("media_type", &self.media_type)?;
        state.serialize_field("bytes", &self.bytes)?;
        state.serialize_field("redaction", &self.redaction)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ArtifactRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("ArtifactRef", ARTIFACT_REF_FIELDS, ArtifactRefVisitor)
    }
}

struct ArtifactRefVisitor;

#[derive(Clone, Copy)]
enum ArtifactRefField {
    Id,
    MediaType,
    Bytes,
    Redaction,
}

impl ArtifactRefField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "id" => Some(Self::Id),
            "media_type" => Some(Self::MediaType),
            "bytes" => Some(Self::Bytes),
            "redaction" => Some(Self::Redaction),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ArtifactRefField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(ArtifactRefFieldVisitor)
    }
}

struct ArtifactRefFieldVisitor;

impl Visitor<'_> for ArtifactRefFieldVisitor {
    type Value = ArtifactRefField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an ArtifactRef field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        ArtifactRefField::from_str(value)
            .ok_or_else(|| E::unknown_field(value, ARTIFACT_REF_FIELDS))
    }
}

impl<'de> Visitor<'de> for ArtifactRefVisitor {
    type Value = ArtifactRef;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a RapidLM ArtifactRef object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut id = None;
        let mut media_type = None;
        let mut bytes = None;
        let mut redaction = None;

        while let Some(field) = access.next_key()? {
            match field {
                ArtifactRefField::Id => assign_once(&mut id, access.next_value()?, "id")?,
                ArtifactRefField::MediaType => {
                    assign_once(&mut media_type, access.next_value()?, "media_type")?;
                }
                ArtifactRefField::Bytes => assign_once(&mut bytes, access.next_value()?, "bytes")?,
                ArtifactRefField::Redaction => {
                    assign_once(&mut redaction, access.next_value()?, "redaction")?;
                }
            }
        }

        Ok(ArtifactRef {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            media_type: media_type.ok_or_else(|| de::Error::missing_field("media_type"))?,
            bytes: bytes.ok_or_else(|| de::Error::missing_field("bytes"))?,
            redaction: redaction.ok_or_else(|| de::Error::missing_field("redaction"))?,
        })
    }
}

impl RedactionClass {
    pub const ALL: &'static [Self] = &[Self::Public, Self::Project, Self::Sensitive, Self::Secret];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Project => "project",
            Self::Sensitive => "sensitive",
            Self::Secret => "secret",
        }
    }
}

impl fmt::Display for RedactionClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RedactionClass {
    type Err = RedactionClassParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for class in Self::ALL {
            if class.as_str() == s {
                return Ok(*class);
            }
        }
        Err(RedactionClassParseError)
    }
}

impl Serialize for RedactionClass {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RedactionClass {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RedactionClassVisitor)
    }
}

struct RedactionClassVisitor;

impl Visitor<'_> for RedactionClassVisitor {
    type Value = RedactionClass;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a snake_case redaction class")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

impl fmt::Display for RedactionClassParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown redaction class")
    }
}

impl Error for RedactionClassParseError {}

fn parse_canonical_artifact_id(s: &str) -> Result<ArtifactId, ArtifactIdParseError> {
    let hex = s
        .strip_prefix(ARTIFACT_ID_PREFIX)
        .ok_or(ArtifactIdParseError)?;
    if hex.len() != ARTIFACT_ID_HEX_LEN {
        return Err(ArtifactIdParseError);
    }
    let hex_bytes = hex.as_bytes();
    let mut digest = [0u8; ARTIFACT_DIGEST_LEN];
    for (i, slot) in digest.iter_mut().enumerate() {
        let hi = hex_nibble(hex_bytes[i * 2])?;
        let lo = hex_nibble(hex_bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(ArtifactId(digest))
}

fn hex_nibble(b: u8) -> Result<u8, ArtifactIdParseError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(ArtifactIdParseError),
    }
}

fn assign_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if slot.is_some() {
        Err(E::duplicate_field(field))
    } else {
        *slot = Some(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // FIPS 180-4 SHA-256 test vectors.
    const EMPTY_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const FOX_HEX: &str = "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592";
    const ABC_ID: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const GOLDEN_PUBLIC: &str = r#"{"id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"text/plain","bytes":3,"redaction":"public"}"#;
    const GOLDEN_SECRET: &str = r#"{"id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"application/octet-stream","bytes":3,"redaction":"secret"}"#;
    const SECRET_CANARY: &str = "super-secret-password";

    fn sha256_id(hex: &str) -> String {
        format!("{ARTIFACT_ID_PREFIX}{hex}")
    }

    #[test]
    fn from_bytes_matches_fips_vectors() {
        assert_eq!(
            ArtifactId::from_bytes(b"").to_string(),
            sha256_id(EMPTY_HEX)
        );
        assert_eq!(
            ArtifactId::from_bytes(b"abc").to_string(),
            sha256_id(ABC_HEX)
        );
        assert_eq!(
            ArtifactId::from_bytes(b"The quick brown fox jumps over the lazy dog").to_string(),
            sha256_id(FOX_HEX)
        );
    }

    #[test]
    fn hash_vectors_are_deterministic() {
        let a = ArtifactId::from_bytes(b"abc");
        let b = ArtifactId::from_bytes(b"abc");
        assert_eq!(a, b);
        assert_eq!(a.to_string(), ABC_ID);
        assert_eq!(a.as_digest(), b.as_digest());
    }

    #[test]
    fn artifact_id_round_trips_display_and_serde() {
        let id = ArtifactId::from_bytes(b"abc");
        assert_eq!(id.to_string().parse::<ArtifactId>().expect("parse"), id);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{ABC_ID}\""));
        let decoded = serde_json::from_str::<ArtifactId>(&json).expect("deserialize");
        assert_eq!(decoded, id);
    }

    #[test]
    fn artifact_id_rejects_non_canonical_forms() {
        for sample in [
            "",
            "abc",
            EMPTY_HEX,
            &format!("SHA256:{ABC_HEX}"),
            &format!("Sha256:{ABC_HEX}"),
            &ABC_HEX.to_ascii_uppercase(),
            &format!("sha256:{}", ABC_HEX.to_ascii_uppercase()),
            &format!("sha256:{ABC_HEX} "),
            &format!("sha256:{ABC_HEX}00"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015a",
            "sha512:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ag",
        ] {
            assert_eq!(
                sample.parse::<ArtifactId>(),
                Err(ArtifactIdParseError),
                "accepted {sample:?}"
            );
            assert!(
                serde_json::from_str::<ArtifactId>(&format!("\"{sample}\"")).is_err(),
                "serde accepted {sample:?}"
            );
        }
    }

    #[test]
    fn golden_artifact_ref_round_trips() {
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            "text/plain",
            3,
            RedactionClass::Public,
        );
        let json = serde_json::to_string(&artifact).expect("serialize");
        assert_eq!(json, GOLDEN_PUBLIC);
        let decoded = serde_json::from_str::<ArtifactRef>(GOLDEN_PUBLIC).expect("deserialize");
        assert_eq!(decoded, artifact);
    }

    #[test]
    fn secret_redaction_class_golden_has_no_payload_field() {
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            "application/octet-stream",
            3,
            RedactionClass::Secret,
        );
        let json = serde_json::to_string(&artifact).expect("serialize");
        assert_eq!(json, GOLDEN_SECRET);
        assert!(!json.contains("content"));
        assert!(!json.contains("plaintext"));
        assert!(!json.contains("payload"));
        let decoded = serde_json::from_str::<ArtifactRef>(GOLDEN_SECRET).expect("deserialize");
        assert_eq!(decoded.redaction, RedactionClass::Secret);
        assert_eq!(decoded, artifact);
    }

    #[test]
    fn secret_bytes_are_not_copied_into_metadata() {
        let secret = SECRET_CANARY.as_bytes();
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(secret),
            "application/octet-stream",
            secret.len() as u64,
            RedactionClass::Secret,
        );
        let json = serde_json::to_string(&artifact).expect("serialize");
        let debug = format!("{artifact:?}");
        assert!(!json.contains(SECRET_CANARY));
        assert!(!debug.contains(SECRET_CANARY));
        assert!(!artifact.id.to_string().contains(SECRET_CANARY));
        assert!(!artifact.media_type.contains(SECRET_CANARY));
        assert_eq!(artifact.redaction, RedactionClass::Secret);
    }

    #[test]
    fn artifact_ref_rejects_secret_payload_fields() {
        for field in ["content", "secret", "plaintext", "payload", "body"] {
            let json = format!(
                r#"{{"id":"{ABC_ID}","media_type":"application/octet-stream","bytes":21,"redaction":"secret","{field}":"{SECRET_CANARY}"}}"#
            );
            assert!(
                serde_json::from_str::<ArtifactRef>(&json).is_err(),
                "accepted smuggled field {field}"
            );
        }
    }

    #[test]
    fn redaction_class_round_trips() {
        for class in RedactionClass::ALL {
            let json = serde_json::to_string(class).expect("serialize");
            assert_eq!(json, format!("\"{}\"", class.as_str()));
            let decoded = serde_json::from_str::<RedactionClass>(&json).expect("deserialize");
            assert_eq!(decoded, *class);
            assert_eq!(
                class.as_str().parse::<RedactionClass>().expect("parse"),
                *class
            );
        }
    }

    #[test]
    fn redaction_class_rejects_unknown() {
        for sample in ["", "SECRET", "Secret", "classified", "public "] {
            assert_eq!(
                sample.parse::<RedactionClass>(),
                Err(RedactionClassParseError)
            );
            assert!(serde_json::from_str::<RedactionClass>(&format!("\"{sample}\"")).is_err());
        }
    }
}
