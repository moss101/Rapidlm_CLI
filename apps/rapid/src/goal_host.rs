//! Durable host-owned goal contract (P6-021).
//!
//! Wraps the single goal-lifecycle authority ([`GoalStateMachine`]) plus the
//! evidence service, and persists the goal snapshot to a project JSON file so a
//! `goal create/show/pause/resume/cancel` command works across invocations. The
//! host owns the contract; completion still requires the evidence gate, and
//! agent-produced evidence must cite a real event-ledger row.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use agent_runtime::{
    BackingError, BackingResolver, CancellationToken, EvidenceError, EvidenceLedgerRef,
    EvidenceRecord, EvidenceService, GoalActor, GoalCommand, GoalEffect, GoalSnapshot,
    GoalStateError, GoalStateMachine,
};
use event_ledger::ledger::{CancellationToken as LedgerCancel, EventLedger, LedgerError};

/// Canonical persisted-goal file name under the project `.rapidlm/` dir.
pub const GOAL_FILE: &str = "goal.json";

/// Canonical persisted evidence doc file name under the project `.rapidlm/` dir.
pub const EVIDENCE_FILE: &str = "goal-evidence.json";

/// Canonical session ledger db name; evidence citations resolve against it.
pub const SESSIONS_DB_FILE: &str = "sessions.sqlite";

/// Read cap for `goal.json` — a single goal snapshot. The schema's own
/// limits (`MAX_CRITERIA` × `MAX_CRITERION_TEXT_BYTES` plus
/// `MAX_GOAL_STATEMENT_BYTES`, `crates/agent-runtime/src/goal/state.rs`)
/// put a legitimate snapshot's ceiling around 272 KiB before JSON
/// structural overhead; this stays comfortably above that so no real goal
/// is ever rejected as oversized.
pub const MAX_GOAL_FILE_BYTES: usize = 1024 * 1024;
/// Read cap for `goal-evidence.json`. Each record can carry several
/// `MAX_CRITERION_TEXT_BYTES`-capped (4 KiB) string fields (assertion,
/// subject, command, criterion id — `crates/agent-runtime/src/evidence.rs`),
/// so `MAX_EVIDENCE_RECORDS` (256) records tops out around 6 MiB before
/// JSON overhead; this stays comfortably above that.
pub const MAX_EVIDENCE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Closed host schema for the persisted evidence doc.
const EVIDENCE_DOC_SCHEMA: &str = "rapidlm.goal_host_evidence";

/// v1 of [`EVIDENCE_DOC_SCHEMA`].
const EVIDENCE_DOC_VERSION: u16 = 1;

/// Ledger event kinds accepted as evidence backing: a completed tool call or
/// job carries a real observed result; other rows do not.
const BACKING_EVENT_KINDS: &[&str] = &["tool.completed", "job.completed"];

/// Resolve evidence citations against the real event ledger. Lives in the
/// composition root because only `apps/rapid` depends on both agent-runtime
/// (the gate) and event-ledger (the durable rows).
struct LedgerEventBacking {
    ledger: EventLedger,
    cancel: LedgerCancel,
}

impl BackingResolver for LedgerEventBacking {
    fn resolve(&self, ledger_ref: &EvidenceLedgerRef) -> Result<(), BackingError> {
        let envelope = self
            .ledger
            .get(ledger_ref.session_id(), ledger_ref.seq(), &self.cancel)
            .map_err(|err| match err {
                LedgerError::EventNotFound { .. } => BackingError::NotFound,
                _ => BackingError::Unavailable,
            })?;
        if envelope.session_id() != ledger_ref.session_id()
            || envelope.event_id().to_string() != ledger_ref.event_id()
        {
            return Err(BackingError::NotFound);
        }
        if BACKING_EVENT_KINDS.contains(&envelope.kind().as_str()) {
            Ok(())
        } else {
            Err(BackingError::NotABackingEvent)
        }
    }
}

/// Typed host persistence failure. Display never echoes goal text.
#[derive(Debug, Eq, PartialEq)]
pub enum GoalPersistError {
    Io,
    Json,
}

impl fmt::Display for GoalPersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => f.write_str("goal store I/O failed"),
            Self::Json => f.write_str("goal store JSON is malformed or unsupported"),
        }
    }
}

impl Error for GoalPersistError {}

/// Durable host-owned goal contract. Completing still requires the evidence gate
/// (`evidence.can_complete`), so the model cannot complete by assertion alone.
pub struct GoalHost {
    machine: GoalStateMachine,
    evidence: EvidenceService,
}

impl GoalHost {
    pub fn new() -> Self {
        Self {
            machine: GoalStateMachine::new(),
            evidence: EvidenceService::new(),
        }
    }

    pub fn from_snapshot(snapshot: GoalSnapshot) -> Self {
        Self {
            machine: GoalStateMachine::from_snapshot(snapshot),
            evidence: EvidenceService::new(),
        }
    }

    /// Install the durable-ledger resolver. Agent-produced evidence must cite
    /// a real ledger row; without this resolver such records never satisfy a
    /// criterion (fail closed). Human and system records are unaffected.
    pub fn install_backing(&mut self, ledger: EventLedger) {
        self.evidence.set_backing_resolver(Arc::new(LedgerEventBacking {
            ledger,
            cancel: LedgerCancel::new(),
        }));
    }

    /// Evidence service view (verdicts, store) for CLI rendering.
    pub fn evidence(&self) -> &EvidenceService {
        &self.evidence
    }

    /// Record one evidence observation. Fails closed on any spec violation.
    pub fn record_evidence(
        &mut self,
        spec: agent_runtime::EvidenceSpec,
    ) -> Result<&EvidenceRecord, EvidenceError> {
        self.evidence.record(spec)
    }

    /// Apply a lifecycle command. Subagent actors are rejected; only a human /
    /// system / main-agent may mutate the top-level contract. `Complete` is
    /// gated on the evidence store (same rule as the agent-facing driver):
    /// every criterion must be satisfied by recorded evidence.
    pub fn apply(
        &mut self,
        command: GoalCommand,
        actor: &GoalActor,
        cancel: &CancellationToken,
    ) -> Result<GoalEffect, GoalStateError> {
        if let GoalCommand::Complete { goal_id } = &command {
            let Some(snapshot) = self.machine.snapshot() else {
                return Err(GoalStateError::NotActive);
            };
            if snapshot.id() != *goal_id {
                return Err(GoalStateError::GoalMismatch {
                    expected: snapshot.id(),
                    found: *goal_id,
                });
            }
            if !self.evidence.can_complete(snapshot).allowed() {
                return Err(GoalStateError::EvidenceMissing);
            }
        }
        self.machine.apply_with_cancel(command, actor, cancel)
    }

    pub fn snapshot(&self) -> Option<&GoalSnapshot> {
        self.machine.snapshot()
    }

    /// Evidence verdicts for the current contract (mandatory criteria, freshness,
    /// deterministic-over-visual). Empty when no goal is active.
    pub fn validate(&self, cancel: &CancellationToken) -> Option<agent_runtime::CriterionVerdicts> {
        let snapshot = self.machine.snapshot()?;
        self.evidence
            .validate_goal_with_cancel(snapshot, cancel)
            .ok()
    }

    pub fn can_complete(&self, cancel: &CancellationToken) -> bool {
        self.snapshot()
            .map(|s| self.evidence.can_complete(s).allowed())
            .unwrap_or(false)
            && !cancel.is_cancelled()
    }

    pub fn save(&self, path: &Path) -> Result<(), GoalPersistError> {
        let Some(snapshot) = self.machine.snapshot() else {
            // A cleared/completed goal no longer persists; drop the stale file so
            // a later `show` reports no active goal instead of resurrecting it.
            match fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) => return Err(GoalPersistError::Io),
            }
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| GoalPersistError::Io)?;
        }
        let json = serde_json::to_string_pretty(snapshot).map_err(|_| GoalPersistError::Json)?;
        fs::write(path, json).map_err(|_| GoalPersistError::Io)
    }

    /// Export the host-owned goal contract + evidence verdicts + a host
    /// attestation digest. `None` when no goal is active. P6-023.
    pub fn export(&self, cancel: &CancellationToken) -> Option<String> {
        let snapshot = self.machine.snapshot()?;
        let snapshot_json = serde_json::to_string(snapshot).ok()?;
        let attestation = protocol::ArtifactId::from_bytes(snapshot_json.as_bytes()).to_string();
        let validated = self.validate(cancel);
        let retry_advisable = validated.as_ref().map(|v| v.retry_advisable());
        let verdicts = validated.as_ref().map(|v| {
            v.verdicts()
                .iter()
                .map(|ve| {
                    serde_json::json!({
                        "criterion_id": ve.criterion_id(),
                        "satisfied": ve.satisfied(),
                        "reason": ve.reason().map(|reason| format!("{reason:?}")),
                        "retryable": ve.reason().map(|reason| reason.retryable()),
                    })
                })
                .collect::<Vec<_>>()
        });
        let doc = serde_json::json!({
            "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot_json).ok(),
            "verdicts": verdicts,
            // Turn-level rollup (Modbit `VER-002`/`AGT-025`, `newtask.md`
            // §2.4): true only when every currently-blocking criterion could
            // resolve without new evidence — see `CriterionVerdicts::
            // retry_advisable`.
            "retry_advisable": retry_advisable,
            "complete": self.can_complete(cancel),
            "attestation": attestation,
        });
        serde_json::to_string_pretty(&doc).ok()
    }

    /// Load a persisted goal. `Ok(None)` when no file exists yet.
    pub fn load(path: &Path) -> Result<Option<Self>, GoalPersistError> {
        // Bound the read itself rather than trusting the file's size on
        // disk — the same stat-then-read gap `read_file_bounded` closes
        // elsewhere in this binary (`.rapidlm/MEMORY.md`, the managed
        // policy doc, the user config). `goal.json` is normally
        // self-written by this host, but corruption, a bad merge, or a
        // file arriving via a cloned repo can still make it arbitrarily
        // large, and this is read unconditionally on every TUI startup and
        // every `rapid goal` subcommand.
        match crate::exec_tools::read_file_bounded(path, MAX_GOAL_FILE_BYTES) {
            Ok(bytes) => {
                let snapshot: GoalSnapshot =
                    serde_json::from_slice(&bytes).map_err(|_| GoalPersistError::Json)?;
                Ok(Some(Self::from_snapshot(snapshot)))
            }
            Err(crate::exec_tools::BoundedReadError::Io(err))
                if err.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(_) => Err(GoalPersistError::Io),
        }
    }

    /// Persist evidence records to the closed host doc. An empty store drops
    /// the stale file (mirrors [`GoalHost::save`]).
    pub fn save_evidence(&self, path: &Path) -> Result<(), GoalPersistError> {
        if self.evidence.store().is_empty() {
            return match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(GoalPersistError::Io),
            };
        }
        let records = self
            .evidence
            .store()
            .records()
            .iter()
            .map(|record| serde_json::to_value(record).map_err(|_| GoalPersistError::Json))
            .collect::<Result<Vec<_>, _>>()?;
        let doc = serde_json::json!({
            "schema": EVIDENCE_DOC_SCHEMA,
            "schema_version": EVIDENCE_DOC_VERSION,
            "records": records,
        });
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| GoalPersistError::Io)?;
        }
        let json = serde_json::to_string_pretty(&doc).map_err(|_| GoalPersistError::Json)?;
        fs::write(path, json).map_err(|_| GoalPersistError::Io)
    }

    /// Restore persisted evidence records. Returns the loaded count (`Ok(0)`
    /// when no doc exists yet). Records decode through the typed evidence
    /// deserializer — the full spec validation — and are bounds-checked on
    /// insert. Ledger citations are re-resolved live at every validation, so
    /// a restored record is only as good as its citation.
    pub fn load_evidence(&mut self, path: &Path) -> Result<usize, GoalPersistError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            records: Vec<EvidenceRecord>,
        }
        // Bound the read itself, same rationale as `GoalHost::load` above.
        match crate::exec_tools::read_file_bounded(path, MAX_EVIDENCE_FILE_BYTES) {
            Ok(bytes) => {
                let raw: Raw =
                    serde_json::from_slice(&bytes).map_err(|_| GoalPersistError::Json)?;
                if raw.schema != EVIDENCE_DOC_SCHEMA || raw.schema_version != EVIDENCE_DOC_VERSION
                {
                    return Err(GoalPersistError::Json);
                }
                let mut count = 0;
                for record in raw.records {
                    self.evidence
                        .restore(record)
                        .map_err(|_| GoalPersistError::Json)?;
                    count += 1;
                }
                Ok(count)
            }
            Err(crate::exec_tools::BoundedReadError::Io(err))
                if err.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(0)
            }
            Err(_) => Err(GoalPersistError::Io),
        }
    }
}

impl Default for GoalHost {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::{
        Criterion, EvidenceKind, EvidenceLedgerRef, EvidenceProducer, EvidenceRequirement,
        EvidenceSpec, EvidenceSource, EvidenceStatus, GoalBudget, GoalCommand, GoalEventKind,
        GoalSpec, TEST_PASSED,
    };
    use protocol::{AgentId, ArtifactId, EvidenceId, GoalId, ProjectId, SessionId};

    fn spec(statement: &str) -> GoalSpec {
        GoalSpec::new(
            GoalId::new(),
            statement,
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec")
    }

    fn spec_requiring(kind: &str) -> GoalSpec {
        GoalSpec::new(
            GoalId::new(),
            "ship auth",
            vec![Criterion::new("c1", "work verified").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec![kind.to_owned()]).expect("req")],
        )
        .expect("spec")
    }

    fn human() -> GoalActor {
        GoalActor::Human
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rapid-goal-host-{}-{}", std::process::id(), name))
    }

    #[test]
    fn create_persist_reload_and_resume() {
        let path = scratch("create");
        let _ = fs::remove_file(&path);

        let mut host = GoalHost::new();
        let created = host
            .apply(GoalCommand::Create(spec("ship auth")), &human(), &CancellationToken::new())
            .expect("create");
        assert_eq!(created.event(), GoalEventKind::Created);
        host.save(&path).expect("save");

        let mut reloaded = GoalHost::load(&path).expect("load").expect("some");
        assert_eq!(reloaded.snapshot().expect("snap").statement(), "ship auth");
        // Lifecycle continues after a reload (durable goal host).
        reloaded
            .apply(
                GoalCommand::Pause { goal_id: reloaded.snapshot().expect("id").id(), process_recovered: false },
                &human(),
                &CancellationToken::new(),
            )
            .expect("pause");
        assert_eq!(
            reloaded.snapshot().expect("snap").state(),
            agent_runtime::GoalState::Paused
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_missing_is_none() {
        let path = scratch("missing");
        let _ = fs::remove_file(&path);
        assert!(GoalHost::load(&path).expect("load").is_none());
    }

    #[test]
    fn load_bounds_the_read_instead_of_buffering_an_oversized_file() {
        // `goal.json` is normally self-written by this host, but corruption,
        // a bad merge, or a file arriving via a cloned repo can still make
        // it arbitrarily large. A garbage file past `MAX_GOAL_FILE_BYTES`
        // gives a genuinely distinguishing signal: an unbounded `fs::read`
        // reads the whole thing and then fails to parse it as JSON
        // (`GoalPersistError::Json`), while the bounded read rejects it as
        // too large before parsing is even attempted
        // (`GoalPersistError::Io`) — the two are different error variants,
        // not just "some error either way".
        let path = scratch("oversized");
        let _ = fs::remove_file(&path);
        fs::write(&path, vec![b'x'; MAX_GOAL_FILE_BYTES + 1]).expect("write");
        match GoalHost::load(&path) {
            Err(GoalPersistError::Io) => {}
            other => panic!("expected Err(Io) for an oversized file, got {}", other.is_ok()),
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn replace_persists_new_contract() {
        let path = scratch("replace");
        let _ = fs::remove_file(&path);

        let mut host = GoalHost::new();
        host.apply(GoalCommand::Create(spec("v1")), &human(), &CancellationToken::new())
            .expect("create");
        host.apply(
            GoalCommand::Replace(spec("v2 contract")),
            &human(),
            &CancellationToken::new(),
        )
        .expect("replace");
        host.save(&path).expect("save");

        let reloaded = GoalHost::load(&path).expect("load").expect("some");
        assert_eq!(reloaded.snapshot().expect("snap").statement(), "v2 contract");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn export_emits_snapshot_verdicts_and_attestation() {
        let mut host = GoalHost::new();
        host.apply(GoalCommand::Create(spec("ship auth")), &human(), &CancellationToken::new())
            .expect("create");
        let export = host.export(&CancellationToken::new()).expect("export has goal");
        let doc: serde_json::Value = serde_json::from_str(&export).expect("json");
        assert_eq!(doc["snapshot"]["statement"], "ship auth");
        assert_eq!(doc["complete"], false);
        assert!(
            doc["attestation"]
                .as_str()
                .expect("attestation")
                .starts_with("sha256:")
        );
        // No evidence recorded yet: `c1` is unsatisfied for a final reason
        // (`missing_evidence`), so `retryable` must read `false`, not
        // `null` — the field is only absent for an already-satisfied
        // criterion (see `CriterionUnsatisfied::retryable`, `newtask.md`
        // §2.4).
        let verdict = &doc["verdicts"][0];
        assert_eq!(verdict["satisfied"], false);
        assert_eq!(verdict["reason"], "MissingEvidence");
        assert_eq!(verdict["retryable"], false);
        // Turn-level rollup: the one blocker isn't retryable, so the whole
        // turn's rollup must read `false` too, not just the per-criterion
        // field.
        assert_eq!(doc["retry_advisable"], false);
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = scratch(name);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn system_test_record(goal_id: GoalId) -> EvidenceSpec {
        EvidenceSpec::new(
            EvidenceId::new(),
            goal_id,
            EvidenceKind::Test,
            TEST_PASSED,
            EvidenceProducer::System,
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-host-evidence")),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command")
    }

    fn agent_record(
        goal_id: GoalId,
        session: SessionId,
        event_id: &str,
        seq: u64,
    ) -> EvidenceSpec {
        let citation = EvidenceLedgerRef::new(session, event_id, seq).expect("ref");
        EvidenceSpec::new(
            EvidenceId::new(),
            goal_id,
            EvidenceKind::Command,
            "command_ran",
            EvidenceProducer::MainAgent {
                agent_id: AgentId::new(),
            },
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-host-agent-evidence")),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command")
        .with_ledger_ref(citation)
    }

    #[test]
    fn evidence_doc_round_trips_and_gates_completion() {
        let dir = scratch_dir("evidence-doc");
        let goal_path = dir.join(GOAL_FILE);
        let evidence_path = dir.join(EVIDENCE_FILE);

        let mut host = GoalHost::new();
        host.apply(
            GoalCommand::Create(spec("ship auth")),
            &human(),
            &CancellationToken::new(),
        )
        .expect("create");
        let goal_id = host.snapshot().expect("snap").id();
        host.record_evidence(system_test_record(goal_id))
            .expect("record");
        // System-produced records are exempt from ledger backing.
        assert!(host.can_complete(&CancellationToken::new()));
        host.save(&goal_path).expect("save goal");
        host.save_evidence(&evidence_path).expect("save evidence");

        let mut reloaded = GoalHost::load(&goal_path).expect("load").expect("some");
        assert_eq!(
            reloaded
                .load_evidence(&evidence_path)
                .expect("load evidence"),
            1
        );
        assert!(reloaded.can_complete(&CancellationToken::new()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn complete_command_is_gated_on_evidence() {
        let mut host = GoalHost::new();
        host.apply(
            GoalCommand::Create(spec("ship auth")),
            &human(),
            &CancellationToken::new(),
        )
        .expect("create");
        let goal_id = host.snapshot().expect("snap").id();
        // No recorded evidence: Complete is refused at the host boundary.
        let err = host
            .apply(
                GoalCommand::Complete { goal_id },
                &human(),
                &CancellationToken::new(),
            )
            .expect_err("incomplete goal cannot complete");
        assert_eq!(err, GoalStateError::EvidenceMissing);
        host.record_evidence(system_test_record(goal_id))
            .expect("record");
        host.apply(
            GoalCommand::Complete { goal_id },
            &human(),
            &CancellationToken::new(),
        )
        .expect("complete");
    }

    #[test]
    fn agent_record_requires_real_ledger_row() {
        use event_ledger::event::{ActorKind, ActorRef, EventKind};
        use event_ledger::ledger::AppendOptions;

        let dir = scratch_dir("agent-backed");
        let db = dir.join(SESSIONS_DB_FILE);
        let ledger = EventLedger::open(&db).expect("open ledger");
        let session = SessionId::new();
        let cancel = LedgerCancel::new();
        ledger
            .create_session(session, ProjectId::new(), &cancel)
            .expect("session row");
        let envelope = ledger
            .append(
                session,
                ActorRef::new(ActorKind::System, &protocol::EventId::new().to_string())
                    .expect("actor"),
                EventKind::ToolCompleted,
                serde_json::json!({"outcome": "passed"}),
                &AppendOptions {
                    redaction: protocol::RedactionClass::Project,
                    trace_id: protocol::TraceId::new(),
                    expected_seq: None,
                },
                &cancel,
            )
            .expect("append tool.completed");
        let event_id = envelope.event_id().to_string();
        let seq = envelope.seq();

        let goal_path = dir.join(GOAL_FILE);
        let evidence_path = dir.join(EVIDENCE_FILE);

        let mut host = GoalHost::new();
        host.install_backing(ledger.clone());
        host.apply(
            GoalCommand::Create(spec_requiring("command")),
            &human(),
            &CancellationToken::new(),
        )
        .expect("create");
        let goal_id = host.snapshot().expect("snap").id();
        host.record_evidence(agent_record(goal_id, session, &event_id, seq))
            .expect("record");
        // Citation resolves against the real ledger row: complete.
        assert!(host.can_complete(&CancellationToken::new()));
        host.save(&goal_path).expect("save goal");
        host.save_evidence(&evidence_path).expect("save evidence");

        // Reload with backing: the citation re-resolves, still complete.
        let mut backed = GoalHost::load(&goal_path).expect("load").expect("some");
        backed.install_backing(EventLedger::open(&db).expect("reopen"));
        backed
            .load_evidence(&evidence_path)
            .expect("load evidence");
        assert!(backed.can_complete(&CancellationToken::new()));

        // Fresh host without a resolver: fail closed despite a valid citation.
        let mut unbacked = GoalHost::load(&goal_path).expect("load").expect("some");
        unbacked
            .load_evidence(&evidence_path)
            .expect("load evidence");
        assert!(!unbacked.can_complete(&CancellationToken::new()));

        // Forged citation (unknown event id) fails the live re-resolution.
        let tampered = fs::read_to_string(&evidence_path)
            .expect("read")
            .replace(&event_id, &protocol::EventId::new().to_string());
        fs::write(&evidence_path, tampered).expect("write tampered");
        let mut forged = GoalHost::load(&goal_path).expect("load").expect("some");
        forged.install_backing(EventLedger::open(&db).expect("reopen"));
        forged.load_evidence(&evidence_path).expect("load evidence");
        assert!(!forged.can_complete(&CancellationToken::new()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_evidence_doc_is_rejected() {
        let dir = scratch_dir("corrupt");
        let evidence_path = dir.join(EVIDENCE_FILE);
        fs::write(&evidence_path, "{ not json").expect("write");
        let mut host = GoalHost::new();
        assert!(host.load_evidence(&evidence_path).is_err());
        fs::write(
            &evidence_path,
            r#"{"schema":"rapidlm.goal_host_evidence","schema_version":1,"records":[]}"#,
        )
        .expect("write");
        assert_eq!(host.load_evidence(&evidence_path).expect("empty doc"), 0);
        fs::remove_dir_all(&dir).ok();
    }
}
