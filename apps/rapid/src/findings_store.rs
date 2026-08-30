//! Persisted, content-hash-keyed dismissal store for scanner findings
//! (Modbit `VER-007`/`VER-008`: dismissed/resolved findings survive reruns
//! keyed by content hash, so a rerun doesn't resurface something already
//! triaged — but changed/new findings are never hidden, since the key is
//! the finding's own content hash, not a line number or a blanket
//! suppression).
//!
//! Read/write failures are never hard errors: a missing or corrupt store is
//! treated as empty (nothing dismissed yet), matching
//! `exec_tools.rs::load_todos`'s established fail-open convention for
//! project-local advisory state. Stored at `.rapidlm/findings.json`,
//! alongside `.rapidlm/todos.json` — project-local, not per-user, since a
//! team's triage decisions are project facts, not personal ones.

use std::collections::BTreeMap;
use std::path::Path;

/// Workspace-relative path of the dismissal store.
pub const FINDINGS_STORE_PATH: &str = ".rapidlm/findings.json";
const FINDINGS_SCHEMA: u32 = 1;

/// One dismissed/resolved finding, keyed by its `FindingFingerprint` hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DismissedFinding {
    pub reason: String,
}

/// In-memory view of the store; call `save` to persist changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindingsStore {
    dismissed: BTreeMap<String, DismissedFinding>,
}

impl FindingsStore {
    /// Load from `root/.rapidlm/findings.json`. Missing or corrupt: empty.
    pub fn load(root: &Path) -> Self {
        let Ok(bytes) = std::fs::read(root.join(FINDINGS_STORE_PATH)) else {
            return Self::default();
        };
        Self::parse(&bytes).unwrap_or_default()
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let entries = value.get("dismissed")?.as_object()?;
        let mut dismissed = BTreeMap::new();
        for (fingerprint, entry) in entries {
            let reason = entry
                .get("reason")
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            dismissed.insert(fingerprint.clone(), DismissedFinding { reason });
        }
        Some(Self { dismissed })
    }

    pub fn is_dismissed(&self, fingerprint_hex: &str) -> bool {
        self.dismissed.contains_key(fingerprint_hex)
    }

    pub fn len(&self) -> usize {
        self.dismissed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dismissed.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &DismissedFinding)> {
        self.dismissed.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Dismiss one fingerprint. Idempotent: re-dismissing overwrites the
    /// reason rather than erroring or duplicating.
    pub fn dismiss(&mut self, fingerprint_hex: &str, reason: &str) {
        self.dismissed.insert(
            fingerprint_hex.to_owned(),
            DismissedFinding {
                reason: reason.to_owned(),
            },
        );
    }

    /// Persist to `root/.rapidlm/findings.json`, creating `.rapidlm/` if
    /// this is the first persisted state for the project.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let document = serde_json::json!({
            "schema": FINDINGS_SCHEMA,
            "dismissed": self
                .dismissed
                .iter()
                .map(|(fingerprint, entry)| {
                    (fingerprint.clone(), serde_json::json!({ "reason": entry.reason }))
                })
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        });
        let target = root.join(FINDINGS_STORE_PATH);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, serde_json::to_vec_pretty(&document)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-findings-store-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn missing_store_is_empty_not_an_error() {
        let root = temp_root("missing");
        let store = FindingsStore::load(&root);
        assert!(store.is_empty());
        assert!(!store.is_dismissed("anything"));
    }

    #[test]
    fn corrupt_store_is_treated_as_empty() {
        let root = temp_root("corrupt");
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        std::fs::write(root.join(FINDINGS_STORE_PATH), b"not json at all").expect("write");
        let store = FindingsStore::load(&root);
        assert!(store.is_empty());
    }

    #[test]
    fn dismiss_persists_and_survives_a_reload() {
        let root = temp_root("roundtrip");
        let mut store = FindingsStore::load(&root);
        store.dismiss("abc123", "test fixture, not a real secret");
        store.save(&root).expect("save");

        let reloaded = FindingsStore::load(&root);
        assert!(reloaded.is_dismissed("abc123"));
        assert!(!reloaded.is_dismissed("def456"));
        assert_eq!(reloaded.len(), 1);
        let (fingerprint, entry) = reloaded.entries().next().expect("one entry");
        assert_eq!(fingerprint, "abc123");
        assert_eq!(entry.reason, "test fixture, not a real secret");
    }

    #[test]
    fn re_dismissing_overwrites_the_reason_not_duplicates() {
        let root = temp_root("overwrite");
        let mut store = FindingsStore::load(&root);
        store.dismiss("abc123", "first reason");
        store.dismiss("abc123", "corrected reason");
        assert_eq!(store.len(), 1);
        let (_, entry) = store.entries().next().expect("one entry");
        assert_eq!(entry.reason, "corrected reason");
    }
}
