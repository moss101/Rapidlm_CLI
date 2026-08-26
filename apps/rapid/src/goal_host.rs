//! Durable host-owned goal contract (P6-021).
//!
//! Wraps the single goal-lifecycle authority ([`GoalStateMachine`]) plus the
//! evidence service, and persists the goal snapshot to a project JSON file so a
//! `goal create/show/pause/resume/cancel` command works across invocations. The
//! host owns the contract; completion still requires the evidence gate.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

use agent_runtime::{
    CancellationToken, EvidenceService, GoalActor, GoalCommand, GoalEffect, GoalSnapshot,
    GoalStateError, GoalStateMachine,
};

/// Canonical persisted-goal file name under the project `.rapidlm/` dir.
pub const GOAL_FILE: &str = "goal.json";

/// Typed host persistence failure. Display never echoes goal text.
#[derive(Debug)]
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

    /// Apply a lifecycle command. Subagent actors are rejected; only a human /
    /// system / main-agent may mutate the top-level contract.
    pub fn apply(
        &mut self,
        command: GoalCommand,
        actor: &GoalActor,
        cancel: &CancellationToken,
    ) -> Result<GoalEffect, GoalStateError> {
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
        let verdicts = self.validate(cancel).map(|v| {
            v.verdicts()
                .iter()
                .map(|ve| {
                    serde_json::json!({
                        "criterion_id": ve.criterion_id(),
                        "satisfied": ve.satisfied(),
                        "reason": ve.reason().map(|reason| format!("{reason:?}")),
                    })
                })
                .collect::<Vec<_>>()
        });
        let doc = serde_json::json!({
            "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot_json).ok(),
            "verdicts": verdicts,
            "complete": self.can_complete(cancel),
            "attestation": attestation,
        });
        serde_json::to_string_pretty(&doc).ok()
    }

    /// Load a persisted goal. `Ok(None)` when no file exists yet.
    pub fn load(path: &Path) -> Result<Option<Self>, GoalPersistError> {
        match fs::read(path) {
            Ok(bytes) => {
                let snapshot: GoalSnapshot =
                    serde_json::from_slice(&bytes).map_err(|_| GoalPersistError::Json)?;
                Ok(Some(Self::from_snapshot(snapshot)))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
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
        Criterion, EvidenceRequirement, GoalBudget, GoalCommand, GoalEventKind, GoalSpec,
    };
    use protocol::GoalId;

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
    }
}
