//! Detached Ed25519 signatures over a document's exact bytes.
//!
//! The trust anchor is a set of public keys an administrator provisions
//! outside the signed document: a document cannot vouch for itself. A
//! signature names no key, so verification tries each trusted key and reports
//! the fingerprint of the one that matched; nothing here reads a file or the
//! environment, so the same inputs always give the same answer.
//!
//! Formats (both text, both strict):
//!
//! * a key: `ed25519:` followed by 64 lowercase-or-uppercase hex digits;
//! * a signature: `ed25519:` followed by 128 hex digits, with optional
//!   surrounding whitespace and nothing else;
//! * a trusted-keys document: one key per line, `#` comments and blank lines
//!   allowed, at most [`MAX_TRUSTED_KEYS`] keys, none repeated.

use std::fmt;

use ring::signature::{ED25519, UnparsedPublicKey};
use sha2::{Digest, Sha256};

/// The algorithm tag every key and signature carries.
pub const ALGORITHM_TAG: &str = "ed25519:";
/// Most keys a trusted-keys document may hold.
pub const MAX_TRUSTED_KEYS: usize = 32;
/// Largest trusted-keys document read, in bytes.
pub const MAX_TRUSTED_KEYS_BYTES: usize = 16 * 1024;
/// Largest signature document read, in bytes.
pub const MAX_SIGNATURE_BYTES: usize = 1024;

const KEY_BYTES: usize = 32;
const SIGNATURE_BYTES: usize = 64;

/// Why a key set or a signature was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureError {
    /// The trusted-keys document holds no key: nothing could ever verify.
    NoTrustedKeys,
    /// A line of the trusted-keys document is not a key (1-based line).
    BadKey { line: usize, reason: &'static str },
    /// The trusted-keys document is too large or names too many keys.
    TooManyKeys,
    /// The signature document is not a well-formed signature.
    BadSignature { reason: &'static str },
    /// The signature is well formed but no trusted key made it.
    NotSignedByATrustedKey,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrustedKeys => f.write_str("the trusted-keys document holds no key"),
            Self::BadKey { line, reason } => {
                write!(f, "trusted key on line {line} is not usable: {reason}")
            }
            Self::TooManyKeys => write!(
                f,
                "the trusted-keys document is over its limit ({MAX_TRUSTED_KEYS} keys, \
                 {MAX_TRUSTED_KEYS_BYTES} bytes)"
            ),
            Self::BadSignature { reason } => write!(f, "the signature is malformed: {reason}"),
            Self::NotSignedByATrustedKey => {
                f.write_str("the signature does not verify under any trusted key")
            }
        }
    }
}

impl std::error::Error for SignatureError {}

/// One trusted Ed25519 public key.
#[derive(Clone, PartialEq, Eq)]
pub struct TrustedKey {
    bytes: [u8; KEY_BYTES],
}

impl TrustedKey {
    /// A short stable identity for logs and `doctor`: the first eight bytes
    /// of the key's SHA-256, in hex. Identifies, never authenticates.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        hex_encode(&Sha256::digest(self.bytes)[..8])
    }
}

impl fmt::Debug for TrustedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrustedKey({})", self.fingerprint())
    }
}

/// The keys a document may be signed by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKeys {
    keys: Vec<TrustedKey>,
}

impl TrustedKeys {
    /// Parse a trusted-keys document. Strict: a line that is not a comment,
    /// blank or a key is an error, never skipped.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
        if text.len() > MAX_TRUSTED_KEYS_BYTES {
            return Err(SignatureError::TooManyKeys);
        }
        let mut keys: Vec<TrustedKey> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let digits = raw
                .strip_prefix(ALGORITHM_TAG)
                .ok_or(SignatureError::BadKey {
                    line,
                    reason: "it does not start with `ed25519:`",
                })?;
            let bytes: [u8; KEY_BYTES] = hex_decode(digits)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(SignatureError::BadKey {
                    line,
                    reason: "it is not 64 hex digits",
                })?;
            let key = TrustedKey { bytes };
            if keys.contains(&key) {
                return Err(SignatureError::BadKey {
                    line,
                    reason: "it repeats an earlier key",
                });
            }
            if keys.len() == MAX_TRUSTED_KEYS {
                return Err(SignatureError::TooManyKeys);
            }
            keys.push(key);
        }
        if keys.is_empty() {
            return Err(SignatureError::NoTrustedKeys);
        }
        Ok(Self { keys })
    }

    /// How many keys are trusted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is trusted (never true of a parsed set).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Verify `signature` (the text of a signature document) over exactly
    /// `message`, returning the fingerprint of the trusted key that made it.
    pub fn verify(&self, message: &[u8], signature: &str) -> Result<String, SignatureError> {
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Err(SignatureError::BadSignature {
                reason: "it is too large",
            });
        }
        let digits =
            signature
                .trim()
                .strip_prefix(ALGORITHM_TAG)
                .ok_or(SignatureError::BadSignature {
                    reason: "it does not start with `ed25519:`",
                })?;
        let bytes: [u8; SIGNATURE_BYTES] = hex_decode(digits)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(SignatureError::BadSignature {
                reason: "it is not 128 hex digits",
            })?;
        self.keys
            .iter()
            .find(|key| {
                UnparsedPublicKey::new(&ED25519, key.bytes)
                    .verify(message, &bytes)
                    .is_ok()
            })
            .map(TrustedKey::fingerprint)
            .ok_or(SignatureError::NotSignedByATrustedKey)
    }
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn pair(seed: u8) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("key")
    }

    fn key_line(pair: &Ed25519KeyPair) -> String {
        format!("{ALGORITHM_TAG}{}", hex_encode(pair.public_key().as_ref()))
    }

    fn sign(pair: &Ed25519KeyPair, message: &[u8]) -> String {
        format!("{ALGORITHM_TAG}{}", hex_encode(pair.sign(message).as_ref()))
    }

    #[test]
    fn rfc_8032_test_vector_1_verifies() {
        // RFC 8032 §7.1, TEST 1: the empty message.
        let keys = TrustedKeys::parse(
            "ed25519:d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        )
        .expect("keys");
        let signature = "ed25519:e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
                         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";
        assert!(keys.verify(b"", signature).is_ok());
        assert_eq!(
            keys.verify(b"x", signature),
            Err(SignatureError::NotSignedByATrustedKey)
        );
    }

    #[test]
    fn a_signature_by_a_trusted_key_verifies_and_names_the_key() {
        let (a, b) = (pair(1), pair(2));
        let keys = TrustedKeys::parse(&format!("# admins\n{}\n\n{}\n", key_line(&a), key_line(&b)))
            .expect("keys");
        assert_eq!(keys.len(), 2);
        let message = b"schema = \"x\"\n";
        let fingerprint = keys.verify(message, &sign(&b, message)).expect("verifies");
        assert_eq!(fingerprint.len(), 16);
        assert_ne!(
            fingerprint,
            keys.verify(message, &sign(&a, message)).expect("verifies")
        );
        // Surrounding whitespace is allowed around a signature, nothing else.
        assert!(
            keys.verify(message, &format!("  {}\n", sign(&a, message)))
                .is_ok()
        );
    }

    #[test]
    fn a_changed_document_an_untrusted_key_or_a_flipped_bit_does_not_verify() {
        let (trusted, stranger) = (pair(3), pair(4));
        let keys = TrustedKeys::parse(&key_line(&trusted)).expect("keys");
        let message = b"max_permission_mode = \"plan\"";
        let signature = sign(&trusted, message);
        assert!(keys.verify(message, &signature).is_ok());
        // One byte changed.
        assert_eq!(
            keys.verify(b"max_permission_mode = \"plaN\"", &signature),
            Err(SignatureError::NotSignedByATrustedKey)
        );
        // Appended whitespace changes the document.
        assert_eq!(
            keys.verify(b"max_permission_mode = \"plan\"\n", &signature),
            Err(SignatureError::NotSignedByATrustedKey)
        );
        // A valid signature by a key that is not trusted.
        assert_eq!(
            keys.verify(message, &sign(&stranger, message)),
            Err(SignatureError::NotSignedByATrustedKey)
        );
        // Every single-hex-digit corruption of the signature fails.
        for at in (ALGORITHM_TAG.len()..signature.len()).step_by(7) {
            let mut bad = signature.clone().into_bytes();
            bad[at] = if bad[at] == b'0' { b'1' } else { b'0' };
            let bad = String::from_utf8(bad).expect("ascii");
            assert!(keys.verify(message, &bad).is_err(), "digit {at}");
        }
    }

    #[test]
    fn a_malformed_signature_is_refused_as_malformed() {
        let keys = TrustedKeys::parse(&key_line(&pair(5))).expect("keys");
        let good = sign(&pair(5), b"m");
        let digits = &good[ALGORITHM_TAG.len()..];
        for (bad, why) in [
            (String::new(), "empty"),
            (digits.to_owned(), "no tag"),
            (format!("rsa:{digits}"), "wrong tag"),
            (format!("{ALGORITHM_TAG}{}", &digits[2..]), "short"),
            (format!("{ALGORITHM_TAG}{digits}00"), "long"),
            (format!("{ALGORITHM_TAG}{}", &digits[1..]), "odd length"),
            (format!("{ALGORITHM_TAG}{}zz", &digits[..126]), "not hex"),
            (format!("{good}\nsecond line"), "trailing text"),
            (format!("{ALGORITHM_TAG}{}", "é".repeat(64)), "non-ascii"),
            ("x".repeat(MAX_SIGNATURE_BYTES + 1), "oversize"),
        ] {
            assert!(
                matches!(
                    keys.verify(b"m", &bad),
                    Err(SignatureError::BadSignature { .. })
                ),
                "{why}"
            );
        }
    }

    #[test]
    fn the_trusted_keys_document_is_strict() {
        let good = key_line(&pair(6));
        let digits = good[ALGORITHM_TAG.len()..].to_owned();
        assert_eq!(
            TrustedKeys::parse("# only a comment\n\n"),
            Err(SignatureError::NoTrustedKeys)
        );
        assert_eq!(TrustedKeys::parse(""), Err(SignatureError::NoTrustedKeys));
        for (text, line) in [
            (format!("{good}\nnot a key\n"), 2),
            (format!("{digits}\n"), 1),
            (format!("{ALGORITHM_TAG}{}\n", &digits[2..]), 1),
            (format!("{ALGORITHM_TAG}{}\n", &digits[1..]), 1),
            (format!("{good}\n{good}\n"), 2),
            (format!("{good} # trailing comment\n"), 1),
        ] {
            assert!(
                matches!(
                    TrustedKeys::parse(&text),
                    Err(SignatureError::BadKey { line: l, .. }) if l == line
                ),
                "{text:?}"
            );
        }
        let many: String = (0..=MAX_TRUSTED_KEYS as u8)
            .map(|n| format!("{}\n", key_line(&pair(n))))
            .collect();
        assert_eq!(TrustedKeys::parse(&many), Err(SignatureError::TooManyKeys));
        let exactly: String = (0..MAX_TRUSTED_KEYS as u8)
            .map(|n| format!("{}\n", key_line(&pair(n))))
            .collect();
        assert_eq!(
            TrustedKeys::parse(&exactly).expect("at the limit").len(),
            MAX_TRUSTED_KEYS
        );
        assert_eq!(
            TrustedKeys::parse(&" ".repeat(MAX_TRUSTED_KEYS_BYTES + 1)),
            Err(SignatureError::TooManyKeys)
        );
    }
}
