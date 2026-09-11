//! P11-001/002: versioned Scenario/Fixture schemas + FixtureManager CAS.

use std::path::{Path, PathBuf};

use protocol::ArtifactId;

use crate::EVAL_SCHEMA;

/// Maximum bytes for one stored fixture body.
pub const MAX_FIXTURE_BYTES: usize = 64 * 1024;

/// A bounded, versioned evaluation scenario.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSpec {
    pub schema: u16,
    pub name: String,
    /// Fixture CAS keys this scenario mounts.
    #[serde(default)]
    pub fixtures: Vec<String>,
    /// Assertion family checks: `family:predicate:expected`.
    #[serde(default)]
    pub assertions: Vec<String>,
}

impl ScenarioSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema: EVAL_SCHEMA,
            name: name.into(),
            fixtures: Vec::new(),
            assertions: Vec::new(),
        }
    }

    pub fn parse(json: &str) -> Result<Self, ScenarioError> {
        let spec: Self = serde_json::from_str(json).map_err(|_| ScenarioError::InvalidSchema)?;
        if spec.schema != EVAL_SCHEMA || spec.name.is_empty() || spec.name.len() > 128 {
            return Err(ScenarioError::InvalidSchema);
        }
        Ok(spec)
    }

    pub fn to_json(&self) -> Result<String, ScenarioError> {
        serde_json::to_string(self).map_err(|_| ScenarioError::InvalidSchema)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenarioError {
    InvalidSchema,
    NotFound,
    TooLarge,
    Io,
}

impl core::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidSchema => "scenario/fixture schema is invalid",
            Self::NotFound => "fixture or scenario not found",
            Self::TooLarge => "fixture exceeds the size bound",
            Self::Io => "fixture store I/O error",
        })
    }
}

impl std::error::Error for ScenarioError {}

/// Content-addressed fixture store: key = sha256 ArtifactId of the bytes.
pub struct FixtureManager {
    root: PathBuf,
}

impl FixtureManager {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    /// Store bytes under their content hash; identical content is idempotent.
    pub fn put(&self, bytes: &[u8]) -> Result<String, ScenarioError> {
        if bytes.len() > MAX_FIXTURE_BYTES {
            return Err(ScenarioError::TooLarge);
        }
        let key = ArtifactId::from_bytes(bytes).to_string();
        let path = self.path_for(&key);
        if path.exists() {
            return Ok(key);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ScenarioError::Io)?;
        }
        std::fs::write(path, bytes).map_err(|_| ScenarioError::Io)?;
        Ok(key)
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>, ScenarioError> {
        // Key must look like a content hash to prevent traversal.
        if !key.starts_with("sha256:") || key.contains("..") {
            return Err(ScenarioError::NotFound);
        }
        std::fs::read(self.path_for(key)).map_err(|_| ScenarioError::NotFound)
    }

    /// Verify integrity: stored bytes must re-hash to the addressed key.
    pub fn verify(&self, key: &str) -> Result<bool, ScenarioError> {
        match self.get(key) {
            Ok(bytes) => Ok(ArtifactId::from_bytes(&bytes).to_string() == key),
            Err(e) => Err(e),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rapidlm-p11-fixtures-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scenario_round_trips_and_rejects_wrong_schema() {
        let mut scenario = ScenarioSpec::new("release-audit");
        scenario.fixtures.push("sha256:abc".into());
        scenario.assertions.push("graph:completed:1".into());
        let json = scenario.to_json().unwrap();
        let parsed = ScenarioSpec::parse(&json).unwrap();
        assert_eq!(parsed, scenario);
        assert_eq!(parsed.schema, crate::EVAL_SCHEMA);

        let bad = r#"{"schema":99,"name":"x"}"#;
        assert!(ScenarioSpec::parse(bad).is_err());
        assert!(ScenarioSpec::parse(r#"{"schema":1,"name":""}"#).is_err());
    }

    #[test]
    fn fixture_cas_is_content_addressed_and_idempotent() {
        let dir = temp("cas");
        let manager = FixtureManager::open(&dir);
        let body = br#"{"seed":42}"#;
        let key = manager.put(body).expect("put");
        assert!(key.starts_with("sha256:"));
        // Idempotent: same content -> same key.
        assert_eq!(manager.put(body).unwrap(), key);
        assert_eq!(manager.get(&key).unwrap(), body.to_vec());
        assert!(manager.verify(&key).unwrap());
        // Tampered store fails verification.
        std::fs::write(manager.path_for(&key), b"tampered").unwrap();
        assert!(!manager.verify(&key).unwrap());
        // Oversized and malformed keys fail closed.
        assert_eq!(
            manager.put(&vec![0u8; MAX_FIXTURE_BYTES + 1]),
            Err(ScenarioError::TooLarge)
        );
        assert_eq!(manager.get("../evil"), Err(ScenarioError::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
