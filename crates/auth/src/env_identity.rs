//! Environment/OS-identity-bound credential cache keying (FR-SEC-007).
//!
//! Short-lived credentials are scoped to the environment that resolved them,
//! so a cache value fetched under one process environment or OS identity is
//! never served to a different one. The fingerprint is computed from the uid
//! plus a canonical (name-sorted) view of environment bindings, so this stays
//! pure and directly testable without an OS keychain.

use std::collections::BTreeMap;

use protocol::ArtifactId;
use serde::{Deserialize, Serialize};

use super::secret::SecretRef;

/// OS user id plus a canonical environment fingerprint.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct EnvIdentity {
    uid: u32,
    fingerprint: ArtifactId,
}

/// Cache key binding a credential handle to the environment that resolved it.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct CredentialCacheKey {
    refer: SecretRef,
    environment: EnvIdentity,
}

impl EnvIdentity {
    /// Compute an identity from a uid and a view of environment name→value
    /// bindings. The fingerprint sorts names so key order cannot change it.
    pub fn from_parts(uid: u32, env: impl IntoIterator<Item = (String, String)>) -> Self {
        let sorted: BTreeMap<String, String> = env.into_iter().collect();
        let mut buf = Vec::new();
        buf.extend_from_slice(&uid.to_be_bytes());
        for (name, value) in &sorted {
            buf.extend_from_slice(&(name.len() as u64).to_be_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(&(value.len() as u64).to_be_bytes());
            buf.extend_from_slice(value.as_bytes());
        }
        Self {
            uid,
            fingerprint: ArtifactId::from_bytes(&buf),
        }
    }

    pub const fn uid(&self) -> u32 {
        self.uid
    }

    pub fn fingerprint(&self) -> ArtifactId {
        self.fingerprint
    }

    /// Bind this environment to a credential handle.
    pub fn cache_key(&self, refer: SecretRef) -> CredentialCacheKey {
        CredentialCacheKey {
            refer,
            environment: self.clone(),
        }
    }
}

impl CredentialCacheKey {
    pub fn refer(&self) -> &SecretRef {
        &self.refer
    }

    pub fn environment(&self) -> &EnvIdentity {
        &self.environment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn same_environment_produces_same_key() {
        let a = EnvIdentity::from_parts(1000, env(&[("HOME", "/home/u"), ("TOKEN", "x")]));
        let b = EnvIdentity::from_parts(1000, env(&[("TOKEN", "x"), ("HOME", "/home/u")]));
        let refer = SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("refer");
        assert_eq!(a, b);
        assert_eq!(a.cache_key(refer.clone()), b.cache_key(refer));
    }

    #[test]
    fn embedded_nul_bytes_cannot_forge_a_different_bindings_identity() {
        // Without length-prefixing, {"a":"1","b":"2"} and {"a":"1\0b\x002"}
        // serialize to the identical delimiter-joined byte string, so a
        // single crafted binding could collide with a genuinely different
        // multi-variable environment's fingerprint.
        let two_bindings = EnvIdentity::from_parts(1000, env(&[("a", "1"), ("b", "2")]));
        let one_crafted_binding = EnvIdentity::from_parts(1000, env(&[("a", "1\0b\x002")]));
        assert_ne!(two_bindings, one_crafted_binding);
    }

    #[test]
    fn different_uid_or_env_produces_different_key() {
        let a = EnvIdentity::from_parts(1000, env(&[("HOME", "/home/u")]));
        let b = EnvIdentity::from_parts(2000, env(&[("HOME", "/home/u")]));
        let c = EnvIdentity::from_parts(1000, env(&[("HOME", "/home/v")]));
        assert_ne!(a, b);
        assert_ne!(a, c);
        let refer = SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("refer");
        assert_ne!(a.cache_key(refer.clone()), b.cache_key(refer.clone()));
        assert_ne!(a.cache_key(refer.clone()), c.cache_key(refer));
    }

    #[test]
    fn fingerprint_is_stable_and_uid_is_carried() {
        let id = EnvIdentity::from_parts(42, env(&[("A", "1")]));
        assert_eq!(id.uid(), 42);
        assert_eq!(id.fingerprint(), id.fingerprint());
        assert_ne!(id.fingerprint(), ArtifactId::from_bytes(b"other"));
    }

    #[test]
    fn identity_and_cache_key_never_expose_raw_env_values() {
        const CANARY: &str = "SUPER_SECRET_ENV_VALUE_abc123";
        let id = EnvIdentity::from_parts(1000, env(&[("AWS_TOKEN", CANARY)]));
        let refer = SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("refer");
        let key = id.cache_key(refer);

        // Identity stores only uid + a content fingerprint: the raw env name and
        // value are never retained, so Debug/Display/serde cannot leak them.
        let debug = format!("{id:?}");
        let key_debug = format!("{key:?}");
        assert!(!debug.contains(CANARY));
        assert!(!debug.contains("AWS_TOKEN"));
        assert!(!key_debug.contains(CANARY));
        assert!(!key_debug.contains("AWS_TOKEN"));

        // Serde of the cache key likewise never contains raw environment text.
        let ser = serde_json::to_string(&key).expect("serialize");
        assert!(!ser.contains(CANARY));
        assert!(!ser.contains("AWS_TOKEN"));
    }
}
