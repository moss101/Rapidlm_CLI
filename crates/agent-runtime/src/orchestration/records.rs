//! Versioned orchestration records (GVS-004).
//!
//! Every durable orchestration payload names its own shape. ADR 0021 asks
//! for `rapidlm.orchestration.<record>/v<n>` and for evolution to be
//! *additive*: a reader that does not know a record skips it, and a reader
//! one version behind still reads the fields it knows. Both halves matter,
//! because the ledger is append-only — a record written today is read by
//! every version that comes after it, and by versions that came before it
//! whenever an older binary opens a newer project.
//!
//! The rules this module enforces:
//!
//! - **A record is self-describing.** [`RecordEnvelope`] carries `record`
//!   (kind and version) beside the body, so a shared event kind — the
//!   ledger has several — can still be told apart without guessing from a
//!   failed parse.
//! - **Unknown fields are ignored, never fatal.** A new writer may add
//!   fields; an old reader keeps working. This is why no orchestration
//!   record uses `deny_unknown_fields`.
//! - **An unknown kind or a future major version is a typed skip**, not an
//!   error: [`RecordEnvelope::read`] returns [`RecordRead::Foreign`] or
//!   [`RecordRead::TooNew`] so a reducer can pass over it and keep going.
//! - **Bodies are bounded.** A record that would exceed
//!   [`MAX_RECORD_BYTES`] is refused at write time; large bodies belong in
//!   the artifact store, referenced by id.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Largest serialized record body. Anything bigger belongs in the artifact
/// store with only its reference in the record. Well under the ledger's own
/// 256 KiB payload bound, so a record that passes here always fits.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;

/// A record kind and the version of its shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RecordKind {
    /// Dotted kind, e.g. `rapidlm.orchestration.candidate`.
    pub name: &'static str,
    /// Major version. A reader refuses a record whose major exceeds the one
    /// it was built for, because a major bump is the one change that is not
    /// additive.
    pub version: u16,
}

impl RecordKind {
    pub const fn new(name: &'static str, version: u16) -> Self {
        Self { name, version }
    }

    /// The wire string, `<name>/v<version>`.
    pub fn wire(&self) -> String {
        format!("{}/v{}", self.name, self.version)
    }

    /// Parse a wire string into its kind name and version.
    fn split(wire: &str) -> Option<(&str, u16)> {
        let (name, version) = wire.rsplit_once("/v")?;
        Some((name, version.parse().ok()?))
    }
}

/// The records this slice defines. Each is the versioned shape ADR 0021's
/// ownership table names; the bodies they wrap live with their owners.
pub const RUN: RecordKind = RecordKind::new("rapidlm.orchestration.run", 1);
pub const FINDING: RecordKind = RecordKind::new("rapidlm.orchestration.finding", 1);
pub const CANDIDATE: RecordKind = RecordKind::new("rapidlm.orchestration.candidate", 1);
pub const CHECK: RecordKind = RecordKind::new("rapidlm.orchestration.check", 1);
pub const VERDICT: RecordKind = RecordKind::new("rapidlm.orchestration.verdict", 1);
pub const BUDGET: RecordKind = RecordKind::new("rapidlm.orchestration.budget", 1);
pub const DECISION: RecordKind = RecordKind::new("rapidlm.orchestration.decision", 1);

/// A durable record: its shape, then its body.
///
/// Deliberately **not** `deny_unknown_fields`. A newer writer adding a field
/// must not break an older reader — that is what "additive" means, and the
/// compatibility test in this module is what keeps it true.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordEnvelope<T> {
    /// `<kind>/v<version>`.
    pub record: String,
    #[serde(flatten)]
    pub body: T,
}

/// What a reader made of a stored record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordRead<T> {
    /// The record is this kind and readable.
    Read(T),
    /// A different kind entirely — another producer's record on a shared
    /// event kind. Skip it; it is not an error.
    Foreign,
    /// The right kind, but a major version this reader was not built for.
    /// Skipped rather than guessed, and reported so the skip is visible.
    TooNew { found: u16, understood: u16 },
}

/// Why a record could not be written or read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordError {
    /// The serialized body exceeds [`MAX_RECORD_BYTES`].
    TooLarge { bytes: usize, limit: usize },
    /// The body could not be encoded or decoded.
    Malformed(String),
}

impl core::fmt::Display for RecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => {
                write!(f, "record is {bytes} bytes, over the {limit}-byte bound")
            }
            Self::Malformed(detail) => write!(f, "malformed record: {detail}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl<T: Serialize> RecordEnvelope<T> {
    /// Wrap a body in its versioned envelope and encode it, refusing a body
    /// that is over the bound.
    pub fn write(kind: RecordKind, body: T) -> Result<serde_json::Value, RecordError> {
        let envelope = RecordEnvelope {
            record: kind.wire(),
            body,
        };
        let value = serde_json::to_value(&envelope)
            .map_err(|err| RecordError::Malformed(err.to_string()))?;
        // `flatten` merges the body's keys into the same map, so a body with
        // its own `record` field would overwrite the kind marker and make
        // the record unreadable as itself — a silent loss, so it is refused.
        if value.get("record").and_then(serde_json::Value::as_str) != Some(kind.wire().as_str()) {
            return Err(RecordError::Malformed(
                "the record body defines its own `record` field, which would displace the kind marker"
                    .to_owned(),
            ));
        }
        let bytes = serde_json::to_vec(&value)
            .map_err(|err| RecordError::Malformed(err.to_string()))?
            .len();
        if bytes > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge {
                bytes,
                limit: MAX_RECORD_BYTES,
            });
        }
        Ok(value)
    }
}

/// Read a stored value as `kind`, skipping anything that is not this
/// reader's business.
pub fn read<T: DeserializeOwned>(
    kind: RecordKind,
    value: &serde_json::Value,
) -> Result<RecordRead<T>, RecordError> {
    let Some(wire) = value.get("record").and_then(serde_json::Value::as_str) else {
        return Ok(RecordRead::Foreign);
    };
    let Some((name, version)) = RecordKind::split(wire) else {
        return Ok(RecordRead::Foreign);
    };
    if name != kind.name {
        return Ok(RecordRead::Foreign);
    }
    if version > kind.version {
        return Ok(RecordRead::TooNew {
            found: version,
            understood: kind.version,
        });
    }
    let body = serde_json::from_value(value.clone())
        .map_err(|err| RecordError::Malformed(err.to_string()))?;
    Ok(RecordRead::Read(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape an older binary was built for.
    #[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
    struct OldBody {
        candidate: String,
        change_count: u32,
    }

    /// The same record after a later slice added a field.
    #[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
    struct NewBody {
        candidate: String,
        change_count: u32,
        #[serde(default)]
        environment_digest: String,
    }

    #[test]
    fn a_record_round_trips_through_its_versioned_envelope() {
        let value = RecordEnvelope::write(
            CANDIDATE,
            OldBody {
                candidate: "cand-1".into(),
                change_count: 3,
            },
        )
        .expect("writes");
        assert_eq!(
            value["record"], "rapidlm.orchestration.candidate/v1",
            "the record names its own shape"
        );
        match read::<OldBody>(CANDIDATE, &value).expect("reads") {
            RecordRead::Read(body) => {
                assert_eq!(body.candidate, "cand-1");
                assert_eq!(body.change_count, 3);
            }
            other => panic!("expected a read, got {other:?}"),
        }
    }

    #[test]
    fn a_new_writers_extra_field_does_not_break_an_old_reader() {
        // The whole point of additive evolution: a field added later must be
        // ignored by a binary that predates it, not refused.
        let written = RecordEnvelope::write(
            CANDIDATE,
            NewBody {
                candidate: "cand-2".into(),
                change_count: 7,
                environment_digest: "sha256:env".into(),
            },
        )
        .expect("writes");
        match read::<OldBody>(CANDIDATE, &written).expect("old reader still reads it") {
            RecordRead::Read(body) => {
                assert_eq!(body.candidate, "cand-2");
                assert_eq!(body.change_count, 7);
            }
            other => panic!("an old reader must read a new record, got {other:?}"),
        }

        // And the reverse: a reader that gained a field reads an older
        // record, defaulting what was not there.
        let old = RecordEnvelope::write(
            CANDIDATE,
            OldBody {
                candidate: "cand-3".into(),
                change_count: 1,
            },
        )
        .expect("writes");
        match read::<NewBody>(CANDIDATE, &old).expect("new reader reads an old record") {
            RecordRead::Read(body) => {
                assert_eq!(body.candidate, "cand-3");
                assert_eq!(body.environment_digest, "", "absent means default");
            }
            other => panic!("expected a read, got {other:?}"),
        }
    }

    #[test]
    fn another_producers_record_is_skipped_not_an_error() {
        // Ledger event kinds are shared, so a reducer meets records it does
        // not own. Those must be passed over, never fail the read.
        let foreign = serde_json::json!({
            "record": "rapidlm.orchestration.verdict/v1",
            "verdict": "verified"
        });
        assert_eq!(
            read::<OldBody>(CANDIDATE, &foreign).expect("skips"),
            RecordRead::Foreign
        );
        // A payload with no `record` marker at all is foreign too.
        let unmarked = serde_json::json!({ "task_id": "x", "round": 1 });
        assert_eq!(
            read::<OldBody>(CANDIDATE, &unmarked).expect("skips"),
            RecordRead::Foreign
        );
    }

    #[test]
    fn a_future_major_version_is_a_typed_skip_not_a_guess() {
        let future = serde_json::json!({
            "record": "rapidlm.orchestration.candidate/v2",
            "candidate": "cand-4",
            "change_count": 2
        });
        assert_eq!(
            read::<OldBody>(CANDIDATE, &future).expect("skips"),
            RecordRead::TooNew {
                found: 2,
                understood: 1
            },
            "a major bump is the one non-additive change, so it is skipped and reported"
        );
    }

    #[test]
    fn a_body_that_would_displace_the_kind_marker_is_refused() {
        #[derive(Serialize)]
        struct Shadow {
            record: &'static str,
        }
        match RecordEnvelope::write(CANDIDATE, Shadow { record: "mine/v9" }) {
            Err(RecordError::Malformed(detail)) => assert!(detail.contains("record"), "{detail}"),
            other => panic!("a shadowing body must be refused, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_body_is_refused_at_write_time() {
        let huge = OldBody {
            candidate: "x".repeat(MAX_RECORD_BYTES + 1),
            change_count: 0,
        };
        match RecordEnvelope::write(CANDIDATE, huge) {
            Err(RecordError::TooLarge { limit, .. }) => assert_eq!(limit, MAX_RECORD_BYTES),
            other => panic!("expected a bound refusal, got {other:?}"),
        }
    }

    #[test]
    fn every_declared_record_has_a_distinct_wire_name() {
        let all = [RUN, FINDING, CANDIDATE, CHECK, VERDICT, BUDGET, DECISION];
        let mut wires: Vec<String> = all.iter().map(RecordKind::wire).collect();
        wires.sort();
        let before = wires.len();
        wires.dedup();
        assert_eq!(before, wires.len(), "record kinds must not collide");
        for wire in &wires {
            assert!(wire.starts_with("rapidlm.orchestration."), "{wire}");
            assert!(wire.ends_with("/v1"), "{wire}");
        }
    }
}
