//! Golden JSON fixture harness for public protocol wire types.
//!
//! `assert_json_fixture("event/v1/tool_completed.json", value)` fails on
//! incompatible drift. Rewrite checked-in fixtures only with an explicit
//! `UPDATE_SCHEMA_FIXTURES=1` environment flag.

#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use protocol::{ApiError, ArtifactId, ArtifactRef, ErrorCode, RedactionClass, SessionId, TraceId};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Explicit rewrite flag. Absent/`0`/`false` never writes fixtures.
const UPDATE_ENV: &str = "UPDATE_SCHEMA_FIXTURES";

/// Cap on a single fixture file. Wire goldens stay small; a huge file is a bug.
const MAX_FIXTURE_BYTES: usize = 1024 * 1024;

const GOLDEN_UUID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

#[derive(Debug)]
enum FixtureError {
    InvalidPath,
    Serialize(serde_json::Error),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Missing {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        bytes: u64,
    },
    NotUtf8 {
        path: PathBuf,
    },
    NotJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    Drift {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath => {
                write!(
                    f,
                    "fixture path must be a relative *.json path under tests/fixtures"
                )
            }
            Self::Serialize(err) => write!(f, "failed to serialize fixture value: {err}"),
            Self::Io { path, source } => {
                write!(f, "fixture I/O failed for {}: {source}", path.display())
            }
            Self::Missing { path } => write!(
                f,
                "missing schema fixture {} (set {UPDATE_ENV}=1 after an explicit compatibility decision)",
                path.display()
            ),
            Self::TooLarge { path, bytes } => write!(
                f,
                "schema fixture {} exceeds {MAX_FIXTURE_BYTES} bytes ({bytes})",
                path.display()
            ),
            Self::NotUtf8 { path } => {
                write!(f, "schema fixture {} is not valid UTF-8", path.display())
            }
            Self::NotJson { path, source } => {
                write!(
                    f,
                    "schema fixture {} is not valid JSON: {source}",
                    path.display()
                )
            }
            Self::Drift {
                path,
                expected,
                actual,
            } => write!(
                f,
                "schema fixture drift at {} (set {UPDATE_ENV}=1 to rewrite after an explicit compatibility decision)\n--- expected\n{expected}--- actual\n{actual}",
                path.display()
            ),
        }
    }
}

impl Error for FixtureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialize(err) | Self::NotJson { source: err, .. } => Some(err),
            Self::Io { source, .. } => Some(source),
            Self::InvalidPath
            | Self::Missing { .. }
            | Self::TooLarge { .. }
            | Self::NotUtf8 { .. }
            | Self::Drift { .. } => None,
        }
    }
}

struct FixtureHarness {
    root: PathBuf,
    update: bool,
}

impl FixtureHarness {
    fn production() -> Self {
        Self {
            root: fixtures_dir(),
            update: update_flag_enabled(env::var(UPDATE_ENV).ok().as_deref()),
        }
    }

    /// Production fixtures, never rewritten. Used by negative tests so an
    /// ambient `UPDATE_SCHEMA_FIXTURES` cannot bless drift or invent schemas.
    fn locked_production() -> Self {
        Self {
            root: fixtures_dir(),
            update: false,
        }
    }

    fn match_json(&self, relative_path: &str, value: &impl Serialize) -> Result<(), FixtureError> {
        let path = resolve_fixture_path(&self.root, relative_path)?;
        let actual = render_json(value)?;
        if self.update {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| FixtureError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(&path, &actual).map_err(|source| FixtureError::Io {
                path: path.clone(),
                source,
            })?;
        }
        if !path.exists() {
            return Err(FixtureError::Missing { path });
        }
        let expected = read_fixture(&path)?;
        if expected != actual {
            return Err(FixtureError::Drift {
                path,
                expected,
                actual,
            });
        }
        Ok(())
    }
}

/// Compare `value` to the checked-in JSON fixture at `relative_path`.
///
/// Never rewrites fixtures unless `UPDATE_SCHEMA_FIXTURES=1` (or `true`/`yes`).
fn assert_json_fixture(relative_path: &str, value: impl Serialize) {
    FixtureHarness::production()
        .match_json(relative_path, &value)
        .unwrap_or_else(|err| panic!("{err}"));
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn update_flag_enabled(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true" | "yes"))
}

fn resolve_fixture_path(root: &Path, relative: &str) -> Result<PathBuf, FixtureError> {
    if relative.is_empty() || !relative.ends_with(".json") || relative.contains('\\') {
        return Err(FixtureError::InvalidPath);
    }
    if Path::new(relative).is_absolute() {
        return Err(FixtureError::InvalidPath);
    }
    // `Path::components()` drops interior `.`, so split the raw string.
    let mut out = PathBuf::new();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(FixtureError::InvalidPath);
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(FixtureError::InvalidPath);
    }
    Ok(root.join(out))
}

fn render_json(value: &impl Serialize) -> Result<String, FixtureError> {
    let mut rendered = serde_json::to_string_pretty(value).map_err(FixtureError::Serialize)?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

fn read_fixture(path: &Path) -> Result<String, FixtureError> {
    let meta = fs::metadata(path).map_err(|source| FixtureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.len() > MAX_FIXTURE_BYTES as u64 {
        return Err(FixtureError::TooLarge {
            path: path.to_path_buf(),
            bytes: meta.len(),
        });
    }
    let bytes = fs::read(path).map_err(|source| FixtureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() > MAX_FIXTURE_BYTES {
        return Err(FixtureError::TooLarge {
            path: path.to_path_buf(),
            bytes: bytes.len() as u64,
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| FixtureError::NotUtf8 {
        path: path.to_path_buf(),
    })?;
    let _: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| FixtureError::NotJson {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(text)
}

fn golden_session_id() -> SessionId {
    SessionId::from_str(GOLDEN_UUID).expect("golden session id")
}

fn golden_api_error() -> ApiError {
    ApiError::new(
        ErrorCode::PolicyDenied,
        "Action denied by project policy",
        TraceId::from_str(GOLDEN_UUID).expect("golden trace"),
    )
    .expect("golden api error")
}

fn golden_artifact_ref() -> ArtifactRef {
    ArtifactRef::new(
        ArtifactId::from_bytes(b"abc"),
        "text/plain",
        3,
        RedactionClass::Public,
    )
}

fn assert_roundtrip<T>(relative_path: &str, value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + fmt::Debug,
{
    assert_json_fixture(relative_path, value);
    let text = fs::read_to_string(fixtures_dir().join(relative_path)).expect("read fixture");
    let decoded: T = serde_json::from_str(&text).expect("decode fixture");
    assert_eq!(&decoded, value);
}

fn unique_temp_root() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!(
        "rapidlm-schema-fixtures-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temp fixtures root");
    root
}

#[test]
fn id_error_and_artifact_fixtures_match_wire_contract() {
    assert_roundtrip("id/v1/session_id.json", &golden_session_id());
    assert_roundtrip("error/v1/api_error.json", &golden_api_error());
    assert_roundtrip("artifact/v1/artifact_ref.json", &golden_artifact_ref());
}

#[test]
fn assert_json_fixture_fails_on_incompatible_event_drift() {
    let value = serde_json::json!({
        "schema": 1,
        "kind": "tool_completed",
    });
    let err = FixtureHarness::locked_production()
        .match_json("event/v1/tool_completed.json", &value)
        .expect_err("unchecked event fixture must not silently pass");
    assert!(
        matches!(
            err,
            FixtureError::Missing { .. } | FixtureError::Drift { .. }
        ),
        "expected missing/drift, got {err}"
    );
}

#[test]
fn incompatible_value_fails_against_checked_in_id_fixture() {
    let other = SessionId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ac").expect("other id");
    let err = FixtureHarness::locked_production()
        .match_json("id/v1/session_id.json", &other)
        .expect_err("drift");
    assert!(
        matches!(err, FixtureError::Drift { .. }),
        "expected drift, got {err}"
    );
}

#[test]
fn fixtures_are_not_rewritten_without_explicit_flag() {
    let root = unique_temp_root();
    let harness = FixtureHarness {
        root: root.clone(),
        update: false,
    };
    let relative = "id/v1/session_id.json";
    let err = harness
        .match_json(relative, &golden_session_id())
        .expect_err("missing fixture");
    assert!(
        matches!(err, FixtureError::Missing { .. }),
        "expected missing, got {err}"
    );
    assert!(
        !root.join(relative).exists(),
        "fixture file was created without {UPDATE_ENV}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_update_flag_writes_then_locks_fixture() {
    let root = unique_temp_root();
    let relative = "id/v1/session_id.json";
    let writer = FixtureHarness {
        root: root.clone(),
        update: true,
    };
    writer
        .match_json(relative, &golden_session_id())
        .expect("explicit update");
    assert!(root.join(relative).is_file());

    let locked = FixtureHarness {
        root: root.clone(),
        update: false,
    };
    locked
        .match_json(relative, &golden_session_id())
        .expect("matches after explicit write");
    let other = SessionId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ac").expect("other id");
    let err = locked.match_json(relative, &other).expect_err("drift");
    assert!(
        matches!(err, FixtureError::Drift { .. }),
        "expected drift, got {err}"
    );
    assert!(
        !locked.update,
        "locked harness must not rewrite on subsequent drift"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn update_flag_requires_explicit_truthy_value() {
    assert!(!update_flag_enabled(None));
    assert!(!update_flag_enabled(Some("")));
    assert!(!update_flag_enabled(Some("0")));
    assert!(!update_flag_enabled(Some("false")));
    assert!(!update_flag_enabled(Some("TRUE")));
    assert!(update_flag_enabled(Some("1")));
    assert!(update_flag_enabled(Some("true")));
    assert!(update_flag_enabled(Some("yes")));
}

#[test]
fn fixture_paths_cannot_escape_root() {
    let harness = FixtureHarness::locked_production();
    for path in [
        "",
        "id/v1/session_id",
        "../Cargo.toml",
        "/etc/passwd",
        "id/../../secret.json",
        "id/./session_id.json",
    ] {
        let err = harness
            .match_json(path, &golden_session_id())
            .expect_err("invalid path");
        assert!(
            matches!(err, FixtureError::InvalidPath),
            "path {path:?} yielded {err}"
        );
    }
}
