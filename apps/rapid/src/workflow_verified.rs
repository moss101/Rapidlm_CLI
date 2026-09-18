//! `rapid run --orchestration verified`: the playbook's steps as a Runtime
//! Graph driven through [`scheduler::GraphBackedRun`].
//!
//! The first production caller of the graph service and the supervisor
//! (ADR 0021, the first Phase 1 slice). What it adds over
//! `orchestration.mode = off`, whose path is untouched:
//!
//! - every step transition is a durable `graph.*` event in the run's ledger
//!   session, appended before the run state changes (the graph service's
//!   own ordering). The graph and the run-state file must agree on what is
//!   ready; a disagreement stops the run instead of picking a side;
//! - the playbook's verification steps are the supervisor's contract — one
//!   mandatory `REQ-<n>`/`AC-<n>` pair per verification step — their real
//!   results are the deterministic checks, and `verified` means the host
//!   supervisor *accepted*: every requirement backed by a passing check run
//!   in this invocation, never a claim. A playbook without a verification
//!   step is refused up front: nothing could be verified;
//! - the run-state file records the graph and session it ran under
//!   ([`OrchestrationRecord`]), the pointer a replay needs.
//!
//! Not yet, and stated rather than emulated: a resumed run opens a fresh
//! graph whose nodes are set to the recorded step states (replay from the
//! events is GVS-005); attempts across invocations are the run state's
//! count; the workspace identity is the digest of the steps' `watch` globs,
//! not a tree identity (GVS-007). Acceptance *is* single-event since
//! GVS-006 — one `orchestration.task_accepted` record that the supervisor
//! and the graph are both reduced from — but only the graph half can be
//! rebuilt from the stream; restoring the supervisor snapshot is GVS-008.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_runtime::orchestration::{
    AcceptanceCriterion, AcceptancePolicy, AgentContextPacket, CandidateCompletion, CheckResult,
    CheckRunner, CheckStatus, CompletionClaim, DiscoveryResult, EvidenceNode, EvidenceTrust,
    Explorer, GapNode, Implementer, OrchestrationBudget, OrchestrationEvidenceKind,
    OrchestrationState, PlanResult, Planner, RequirementClaim, RequirementClaimStatus,
    RequirementNode, RequirementPriority, RequirementVerificationStatus, Retriever, Strategist,
    StrategyRevision, SupervisorDrivers, SupervisorError, TaskComplexity, TaskContract,
    TransitionError, Verdict, VerificationCheck, VerificationPolicy, WorkspaceIdentity,
    WorkspacePolicy,
};
use event_ledger::ledger::EventLedger;
use protocol::{ArtifactId, EvidenceId, GoalId, NodeId, ProjectId, SessionId};
use scheduler::graph::EdgeCondition;
use scheduler::kinds::{EdgeKind, NodeKind, NodeState};
use scheduler::{EdgeSpec, GraphBackedRun, GraphError, NodeSpec, RunPlan};

use serde::Deserialize as _;

use crate::workflow::{PlaybookFile, RunState, Step, StepState, evidence_digest};

/// Longest verification command an evidence record can cite
/// (`agent_runtime::evidence::MAX_COMMAND_BYTES`). Checked when the run
/// opens so a run never fails at acceptance over a bound it could have
/// reported at the start.
const MAX_COMMAND_BYTES: usize = agent_runtime::evidence::MAX_COMMAND_BYTES;

/// Byte cap on the text an evidence record or check summary carries.
const MAX_SUMMARY_BYTES: usize = 1024;

/// Why a verified run could not be opened or driven. Every variant is a
/// stop: the run's step results stay recorded, nothing is accepted.
#[derive(Debug)]
pub enum VerifiedRunError {
    /// The playbook declares no `verification` step, so there is nothing a
    /// supervisor could verify.
    NoVerificationSteps,
    /// A verification step's command is longer than an evidence record can
    /// cite.
    CommandTooLong { key: String, max_bytes: usize },
    /// The graph service refused (ledger append failed, proposal invalid).
    Graph(GraphError),
    /// The supervisor refused a transition or the contract.
    Supervisor(SupervisorError),
    /// The graph and the run state disagree about what is ready.
    Disagreement(String),
    /// A step key the playbook does not declare.
    UnknownStep(String),
}

impl core::fmt::Display for VerifiedRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoVerificationSteps => f.write_str(
                "the playbook has no verification step; verified orchestration needs at least one",
            ),
            Self::CommandTooLong { key, max_bytes } => write!(
                f,
                "verification step '{key}': command exceeds {max_bytes} bytes"
            ),
            Self::Graph(err) => write!(f, "graph: {err}"),
            Self::Supervisor(err) => write!(f, "supervisor: {err}"),
            Self::Disagreement(detail) => write!(f, "graph and run state disagree: {detail}"),
            Self::UnknownStep(key) => write!(f, "unknown step '{key}'"),
        }
    }
}

impl std::error::Error for VerifiedRunError {}

impl From<GraphError> for VerifiedRunError {
    fn from(err: GraphError) -> Self {
        Self::Graph(err)
    }
}

impl From<SupervisorError> for VerifiedRunError {
    fn from(err: SupervisorError) -> Self {
        Self::Supervisor(err)
    }
}

/// What the run-state file records about its verified orchestration: the
/// graph and ledger session the transitions were appended to, and where
/// the supervisor got to. `accepted` is the only way a verified run reports
/// `verified: true`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrchestrationRecord {
    pub mode: String,
    pub session: String,
    pub graph_id: String,
    pub task_id: String,
    pub state: String,
    pub accepted: bool,
    /// The supervisor snapshot this run reached, so a later invocation can
    /// reduce it back rather than starting a fresh run that has forgotten
    /// what the last one established (GVS-008). Absent on a record written
    /// before the snapshot was serializable, which simply starts fresh.
    ///
    /// Read leniently: a snapshot this binary cannot decode must not make
    /// the whole run state unloadable — the run itself is still resumable
    /// from its step records, and losing the ability to recover the
    /// supervisor is far better than losing the run.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_snapshot"
    )]
    pub snapshot: Option<agent_runtime::orchestration::OrchestrationSnapshot>,
}

/// Decode a persisted snapshot, treating one this binary cannot read as
/// absent rather than failing the whole `RunState`.
fn lenient_snapshot<'de, D>(
    deserializer: D,
) -> Result<Option<agent_runtime::orchestration::OrchestrationSnapshot>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| serde_json::from_value(value).ok()))
}

/// The supervisor's verdict on a finished run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conclusion {
    pub accepted: bool,
    pub state: OrchestrationState,
    pub verdict: Verdict,
    /// Verification steps whose requirement the host did not find
    /// satisfied — no passing check ran for them in this invocation.
    pub unmet: Vec<String>,
}

/// A verification step's captured result, keyed by step key, for the
/// supervisor's check runner to replay. The checks ran once, for real, as
/// graph steps; the supervisor's audit trail replays those results and
/// never executes a second time (the same rule `goal claim` follows).
type CapturedChecks = Arc<Mutex<BTreeMap<String, CheckResult>>>;

/// One `rapid run` under verified orchestration. Created before the first
/// step runs, consulted at every transition, concluded once no step is
/// ready.
pub struct VerifiedRun {
    run: GraphBackedRun,
    task_id: GoalId,
    /// Step key → graph node.
    nodes: BTreeMap<String, NodeId>,
    /// Verification step key → the contract requirement it is.
    requirements: BTreeMap<String, String>,
    checks: CapturedChecks,
    evidence: BTreeMap<String, EvidenceNode>,
    accepted: bool,
}

impl VerifiedRun {
    /// Open the run's graph and supervisor over `playbook`, mirroring the
    /// step states `state` already records (a resumed run), and advance the
    /// supervisor through its host-owned phases to `Implementing`.
    pub fn open(
        playbook: &PlaybookFile,
        state: &RunState,
        root: &Path,
        ledger: EventLedger,
        session: SessionId,
    ) -> Result<Self, VerifiedRunError> {
        let verification: Vec<(usize, &Step)> = playbook
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.kind == NodeKind::Verification)
            .collect();
        if verification.is_empty() {
            return Err(VerifiedRunError::NoVerificationSteps);
        }
        for (_, step) in &verification {
            let command = step.command.as_deref().unwrap_or("");
            if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
                return Err(VerifiedRunError::CommandTooLong {
                    key: step.key.clone(),
                    max_bytes: MAX_COMMAND_BYTES,
                });
            }
        }

        let task_id = GoalId::new();
        let requirements: BTreeMap<String, String> = verification
            .iter()
            .map(|(index, step)| (step.key.clone(), format!("REQ-{index}")))
            .collect();
        let contract = contract(task_id, playbook, &verification)?;
        let identity = workspace_identity(root, playbook);
        let checks: CapturedChecks = Arc::new(Mutex::new(BTreeMap::new()));
        let drivers = SupervisorDrivers {
            planner: Box::new(PlaybookPhases {
                steps: playbook.steps.iter().map(|s| s.label.clone()).collect(),
                files: bounded_root_listing(root),
            }),
            explorer: Box::new(PlaybookPhases {
                steps: Vec::new(),
                files: bounded_root_listing(root),
            }),
            retriever: Box::new(PlaybookPhases {
                steps: Vec::new(),
                files: bounded_root_listing(root),
            }),
            implementer: Box::new(HostBlocked),
            verifiers: Vec::new(),
            strategist: Box::new(HostBlocked),
            checks: Box::new(ReplayedChecks {
                results: Arc::clone(&checks),
            }),
        };

        let nodes: BTreeMap<String, NodeId> = playbook
            .steps
            .iter()
            .map(|step| (step.key.clone(), NodeId::new()))
            .collect();
        let plan = RunPlan {
            nodes: playbook
                .steps
                .iter()
                .map(|step| NodeSpec {
                    id: nodes[&step.key],
                    kind: step.kind,
                    label: step.label.clone(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: step.budget_tokens,
                    max_attempts: step.max_attempts.max(1),
                })
                .collect(),
            edges: playbook
                .steps
                .iter()
                .flat_map(|step| {
                    step.depends_on.iter().map(|dep| EdgeSpec {
                        from: nodes[dep],
                        to: nodes[&step.key],
                        kind: EdgeKind::DependsOn,
                        condition: EdgeCondition::default(),
                    })
                })
                .collect(),
        };
        let mut run = GraphBackedRun::start_planned(
            contract,
            identity,
            drivers,
            ledger,
            session,
            ProjectId::new(),
            plan,
        )?;

        // Host-owned phases only, exactly as `goal claim` drives them: the
        // implementer is the run itself, so stop at `Implementing`.
        loop {
            match run.orchestration_state() {
                OrchestrationState::Implementing => break,
                OrchestrationState::Contracting
                | OrchestrationState::Discovering
                | OrchestrationState::Planning
                | OrchestrationState::Retrieving
                | OrchestrationState::ReadyToImplement => {
                    run.supervisor.advance()?;
                }
                _ => {
                    return Err(
                        SupervisorError::Transition(TransitionError::InvalidTransition).into(),
                    );
                }
            }
        }

        let mut this = Self {
            run,
            task_id,
            nodes,
            requirements,
            checks,
            evidence: BTreeMap::new(),
            accepted: false,
        };
        this.mirror(playbook, state)?;
        Ok(this)
    }

    /// Set the fresh graph's nodes to the step states the run-state file
    /// already records, so a resumed run's graph agrees with it before the
    /// first readiness check. Terminal steps are not re-run; a failed step
    /// with attempts left is re-queued the way the loop would.
    fn mirror(
        &mut self,
        playbook: &PlaybookFile,
        state: &RunState,
    ) -> Result<(), VerifiedRunError> {
        for step in &playbook.steps {
            let node = self.node(&step.key)?;
            match state.steps.get(&step.key) {
                Some(StepState::Pending) | None => {}
                Some(StepState::Running) => {
                    self.run
                        .graphs
                        .set_state(self.run.graph_id, node, NodeState::Running)?;
                }
                Some(StepState::Succeeded) => {
                    self.run
                        .graphs
                        .set_state(self.run.graph_id, node, NodeState::Succeeded)?;
                }
                Some(StepState::Failed { .. }) => {
                    self.run
                        .graphs
                        .set_state(self.run.graph_id, node, NodeState::Failed)?;
                    // The same rule the run loop applies, so the graph's
                    // ready set matches the run state's: a failure with an
                    // attempt left is re-queued, a final one is not.
                    if !crate::workflow::failed_for_good(state, step) {
                        self.run.graphs.retry(self.run.graph_id, node)?;
                    }
                }
                Some(StepState::Cancelled) => {
                    self.run
                        .graphs
                        .set_state(self.run.graph_id, node, NodeState::Cancelled)?;
                }
                Some(StepState::WaitingHuman { wait_token }) => {
                    self.run.graphs.wait(self.run.graph_id, node, wait_token)?;
                }
            }
        }
        Ok(())
    }

    fn node(&self, key: &str) -> Result<NodeId, VerifiedRunError> {
        self.nodes
            .get(key)
            .copied()
            .ok_or_else(|| VerifiedRunError::UnknownStep(key.to_owned()))
    }

    /// The steps the graph considers ready, in playbook order.
    pub fn ready_steps<'a>(
        &self,
        playbook: &'a PlaybookFile,
    ) -> Result<Vec<&'a Step>, VerifiedRunError> {
        let snapshot = self.run.graphs.snapshot(self.run.graph_id)?;
        let ready = snapshot.ready_set();
        Ok(playbook
            .steps
            .iter()
            .filter(|step| {
                self.nodes
                    .get(&step.key)
                    .is_some_and(|id| ready.contains(id))
            })
            .collect())
    }

    /// Stop unless the graph's ready set is exactly `expected` (the run
    /// state's). The two are mirrors; when they diverge the run has a bug
    /// and must not guess which one to trust.
    pub fn confirm_ready(
        &self,
        playbook: &PlaybookFile,
        expected: &[&Step],
    ) -> Result<(), VerifiedRunError> {
        let graph: Vec<&str> = self
            .ready_steps(playbook)?
            .into_iter()
            .map(|step| step.key.as_str())
            .collect();
        let mut state: Vec<&str> = expected.iter().map(|step| step.key.as_str()).collect();
        state.sort_unstable();
        let mut graph_sorted = graph.clone();
        graph_sorted.sort_unstable();
        if graph_sorted == state {
            Ok(())
        } else {
            Err(VerifiedRunError::Disagreement(format!(
                "graph ready {graph:?}, run state ready {state:?}"
            )))
        }
    }

    pub fn step_started(&mut self, key: &str) -> Result<(), VerifiedRunError> {
        let node = self.node(key)?;
        self.run
            .graphs
            .set_state(self.run.graph_id, node, NodeState::Running)?;
        Ok(())
    }

    /// A step finished with exit 0 / a succeeded turn. A verification
    /// step's result is captured as the deterministic check the supervisor
    /// replays and the evidence its requirement cites.
    pub fn step_succeeded(&mut self, step: &Step, output: &str) -> Result<(), VerifiedRunError> {
        let node = self.node(&step.key)?;
        self.run
            .graphs
            .set_state(self.run.graph_id, node, NodeState::Succeeded)?;
        if step.kind != NodeKind::Verification {
            return Ok(());
        }
        let evidence_id = EvidenceId::new();
        let observed_at = self.run.graphs.last_seq()?;
        self.capture_check(CheckResult {
            check_id: step.key.clone(),
            status: CheckStatus::Passed,
            exit_code: Some(0),
            duration_ms: 0,
            stdout_ref: None,
            stderr_ref: None,
            evidence_id,
            failure_summary: None,
        });
        let requirement = self
            .requirements
            .get(&step.key)
            .cloned()
            .ok_or_else(|| VerifiedRunError::UnknownStep(step.key.clone()))?;
        self.evidence.insert(
            step.key.clone(),
            EvidenceNode {
                id: evidence_id,
                task_id: self.task_id,
                requirement_ids: vec![requirement],
                producer: "rapid run".into(),
                kind: OrchestrationEvidenceKind::Tool,
                source: step.key.clone(),
                timestamp: observed_at,
                artifact_ref: None,
                command_ref: step.command.clone(),
                workspace_ref: None,
                content_hash: Some(ArtifactId::from_bytes(output.as_bytes()).to_string()),
                summary: bounded(&format!("verification step '{}' passed (exit 0)", step.key)),
                structured_payload: None,
                trust_level: EvidenceTrust::Deterministic,
            },
        );
        Ok(())
    }

    /// A step failed. `will_retry` re-queues it on the graph the way the
    /// run loop will re-run it; otherwise the failure is final on the graph
    /// too. A verification step's failure is captured as a failed check —
    /// any later pass replaces it.
    pub fn step_failed(
        &mut self,
        step: &Step,
        reason: &str,
        will_retry: bool,
    ) -> Result<(), VerifiedRunError> {
        let node = self.node(&step.key)?;
        self.run
            .graphs
            .set_state(self.run.graph_id, node, NodeState::Failed)?;
        if step.kind == NodeKind::Verification {
            self.capture_check(CheckResult {
                check_id: step.key.clone(),
                status: CheckStatus::Failed,
                exit_code: None,
                duration_ms: 0,
                stdout_ref: None,
                stderr_ref: None,
                evidence_id: EvidenceId::new(),
                failure_summary: Some(bounded(reason)),
            });
            self.evidence.remove(&step.key);
        }
        if will_retry {
            self.run.graphs.retry(self.run.graph_id, node)?;
        }
        Ok(())
    }

    pub fn step_cancelled(&mut self, key: &str) -> Result<(), VerifiedRunError> {
        let node = self.node(key)?;
        self.run
            .graphs
            .set_state(self.run.graph_id, node, NodeState::Cancelled)?;
        Ok(())
    }

    /// A human step is waiting on a durable approval token.
    pub fn step_waiting(&mut self, key: &str, wait_token: &str) -> Result<(), VerifiedRunError> {
        let node = self.node(key)?;
        self.run.graphs.wait(self.run.graph_id, node, wait_token)?;
        Ok(())
    }

    fn capture_check(&mut self, result: CheckResult) {
        if let Ok(mut map) = self.checks.lock() {
            map.insert(result.check_id.clone(), result);
        }
    }

    /// No step is ready: submit the run as the candidate, replay its checks,
    /// take the host verdict and — only if verified — accept. The outcome
    /// is reported, never assumed: a step that passed in an earlier
    /// invocation has no fresh check here and leaves its requirement unmet.
    pub fn conclude(&mut self, playbook: &PlaybookFile) -> Result<Conclusion, VerifiedRunError> {
        let mut claims = Vec::new();
        let mut evidence_refs = Vec::new();
        let mut checks_requested = Vec::new();
        let mut passed = 0usize;
        for step in playbook
            .steps
            .iter()
            .filter(|step| step.kind == NodeKind::Verification)
        {
            let requirement = self
                .requirements
                .get(&step.key)
                .cloned()
                .ok_or_else(|| VerifiedRunError::UnknownStep(step.key.clone()))?;
            checks_requested.push(step.key.clone());
            match self.evidence.get(&step.key).cloned() {
                Some(node) => {
                    self.run.supervisor.record_evidence(node.clone())?;
                    evidence_refs.push(node.id);
                    passed += 1;
                    claims.push(RequirementClaim {
                        requirement_id: requirement,
                        claimed_status: RequirementClaimStatus::Satisfied,
                        evidence_refs: vec![node.id],
                        explanation: "verification step passed in this invocation".into(),
                    });
                }
                None => claims.push(RequirementClaim {
                    requirement_id: requirement,
                    claimed_status: RequirementClaimStatus::Unsatisfied,
                    evidence_refs: Vec::new(),
                    explanation: "no passing verification step in this invocation".into(),
                }),
            }
        }
        let total = checks_requested.len();
        let candidate = CandidateCompletion {
            task_id: self.task_id,
            summary: bounded(&format!(
                "rapid run '{}': {passed} of {total} verification step(s) passed in this invocation",
                playbook.name
            )),
            changed_artifacts: Vec::new(),
            requirement_claims: claims,
            evidence_refs,
            checks_requested,
            known_limitations: Vec::new(),
            unresolved_items: Vec::new(),
            completion_claim: if passed == total {
                CompletionClaim::Done
            } else {
                CompletionClaim::Partial
            },
        };
        self.run.supervisor.submit_candidate(candidate)?;
        self.run.supervisor.run_checks()?;
        let verdict = self.run.supervisor.verify()?;
        if self.run.orchestration_state() == OrchestrationState::Verified {
            self.run.accept()?;
            self.accepted = true;
        }
        let by_requirement: BTreeMap<&str, &str> = self
            .requirements
            .iter()
            .map(|(key, requirement)| (requirement.as_str(), key.as_str()))
            .collect();
        let mut unmet: Vec<String> = verdict
            .requirement_results
            .iter()
            .filter(|result| result.status != RequirementVerificationStatus::Satisfied)
            .filter_map(|result| by_requirement.get(result.requirement_id.as_str()))
            .map(|key| (*key).to_owned())
            .collect();
        unmet.sort_unstable();
        unmet.dedup();
        Ok(Conclusion {
            accepted: self.accepted,
            state: self.run.orchestration_state(),
            verdict: verdict.verdict,
            unmet,
        })
    }

    /// What the run-state file records about this orchestration.
    pub fn record(&self) -> OrchestrationRecord {
        OrchestrationRecord {
            mode: protocol::OrchestrationMode::Verified.as_str().to_owned(),
            session: self.run.graphs.session().to_string(),
            graph_id: self.run.graph_id.to_string(),
            task_id: self.task_id.to_string(),
            state: format!("{:?}", self.run.orchestration_state()),
            accepted: self.accepted,
            snapshot: Some(self.run.supervisor.snapshot().clone()),
        }
    }

    /// What a previous invocation's supervisor had reached, recovered as
    /// paused — the reducer half of GVS-008 applied to this run. `None` when
    /// there is nothing to recover, in which case the run starts fresh.
    ///
    /// Recovering does not resume: the restored run is `Paused`, and the
    /// caller decides when (or whether) to re-enter the phase it was
    /// interrupted in, because an interrupted run may have left effects that
    /// a reconciliation has to settle first.
    pub fn recovered_state(state: &RunState) -> Result<Option<OrchestrationState>, String> {
        let Some(snapshot) = state
            .orchestration
            .as_ref()
            .and_then(|record| record.snapshot.clone())
        else {
            return Ok(None);
        };
        let recovered = agent_runtime::orchestration::Supervisor::recover(
            snapshot,
            SupervisorDrivers {
                planner: Box::new(HostBlocked),
                explorer: Box::new(HostBlocked),
                retriever: Box::new(HostBlocked),
                implementer: Box::new(HostBlocked),
                verifiers: Vec::new(),
                strategist: Box::new(HostBlocked),
                checks: Box::new(ReplayedChecks {
                    results: Arc::new(Mutex::new(BTreeMap::new())),
                }),
            },
        )
        // Swallowing this would report "nothing to recover" for a run that
        // genuinely left effects in flight, and it would then restart fresh
        // — the hazard `Paused` exists to prevent.
        .map_err(|err| format!("the recorded run could not be recovered: {err}"))?;
        Ok(Some(recovered.state()))
    }

    pub fn graph_id(&self) -> protocol::GraphId {
        self.run.graph_id
    }

    /// Step key → graph node, for readers of the run's events.
    pub fn nodes(&self) -> &BTreeMap<String, NodeId> {
        &self.nodes
    }

    pub fn session(&self) -> SessionId {
        self.run.graphs.session()
    }
}

/// The run's contract: one mandatory requirement per verification step.
/// Ids are the step's position in the playbook (`REQ-<n>`/`AC-<n>`), which
/// is stable across resumes and always a valid contract id — step keys
/// admit `.` and `/`, contract ids do not.
fn contract(
    task_id: GoalId,
    playbook: &PlaybookFile,
    verification: &[(usize, &Step)],
) -> Result<TaskContract, VerifiedRunError> {
    let text = |step: &Step| {
        bounded(&format!(
            "verification step '{}' ({}) exits 0",
            step.key, step.label
        ))
    };
    let requirements = verification
        .iter()
        .map(|(index, step)| RequirementNode {
            id: format!("REQ-{index}"),
            description: text(step),
            source: "playbook".into(),
            priority: RequirementPriority::Normal,
            mandatory: true,
            depends_on: Vec::new(),
            acceptance_criteria: vec![format!("AC-{index}")],
        })
        .collect();
    let acceptance_criteria = verification
        .iter()
        .map(|(index, step)| AcceptanceCriterion {
            id: format!("AC-{index}"),
            text: text(step),
        })
        .collect();
    // The contract's wall-clock budget is the sum of every step's ceiling
    // times its attempts — what the playbook itself allows. It is recorded,
    // not enforced: only `Supervisor::advance` checks the budget, and this
    // run advances only before its steps run.
    let wall_clock_ms: u64 = playbook
        .steps
        .iter()
        .map(|step| {
            step.timeout_secs
                .unwrap_or(crate::workflow::DEFAULT_STEP_TIMEOUT_SECS)
                .saturating_mul(u64::from(step.max_attempts.max(1)))
                .saturating_mul(1000)
        })
        .fold(60_000u64, u64::saturating_add);
    let contract = TaskContract {
        id: task_id,
        parent_task_id: None,
        objective: bounded(&format!("rapid run: {}", playbook.name)),
        scope: Vec::new(),
        constraints: Vec::new(),
        requirements,
        acceptance_criteria,
        permitted_capabilities: Vec::new(),
        required_capabilities: Vec::new(),
        forbidden_actions: Vec::new(),
        expected_outputs: Vec::new(),
        verification_policy: VerificationPolicy {
            deterministic_checks_required: true,
            skeptic_count: 0,
            max_rounds: 1,
            strategist_after: 0,
            acceptance_policy: AcceptancePolicy::Strict,
            minimum_requirement_coverage: 100,
            require_workspace_identity: false,
            allow_inconclusive_acceptance: false,
            complexity: TaskComplexity::Standard,
        },
        resource_budget: OrchestrationBudget {
            max_verification_rounds: 1,
            max_repair_rounds: 0,
            max_strategist_calls: 0,
            max_agent_spawns: 0,
            max_tokens: 0,
            max_wall_clock_ms: wall_clock_ms,
            max_tool_executions: 0,
        },
        context_budget: 0,
        workspace_policy: WorkspacePolicy::Direct,
        complexity: TaskComplexity::Standard,
        metadata: std::collections::BTreeSet::new(),
    };
    contract
        .validate()
        .map_err(|err| VerifiedRunError::Supervisor(SupervisorError::InvalidContract(err)))?;
    Ok(contract)
}

/// The Phase 1 stand-in for a tree identity: the digest of every file the
/// playbook's `watch` globs cover — the same evidence scope the run's
/// invalidation uses. A playbook without `watch` globs digests nothing.
fn workspace_identity(root: &Path, playbook: &PlaybookFile) -> WorkspaceIdentity {
    let mut globs: Vec<String> = playbook
        .steps
        .iter()
        .flat_map(|step| step.watch.iter().cloned())
        .collect();
    globs.sort_unstable();
    globs.dedup();
    WorkspaceIdentity {
        hash: evidence_digest(root, &globs),
    }
}

/// Bounded top-level listing for the discovery phase (the same bound
/// `goal claim` uses).
fn bounded_root_listing(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .take(16)
        .collect();
    names.sort();
    names
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_SUMMARY_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_SUMMARY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Host-owned discovery/planning derived from the playbook itself; no
/// model is consulted.
struct PlaybookPhases {
    steps: Vec<String>,
    files: Vec<String>,
}

impl Planner for PlaybookPhases {
    fn plan(&self, _packet: &AgentContextPacket) -> Result<PlanResult, SupervisorError> {
        Ok(PlanResult {
            summary: "the playbook's declared steps".into(),
            steps: self.steps.clone(),
        })
    }
}

impl Explorer for PlaybookPhases {
    fn explore(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "bounded project listing".into(),
            files: self.files.clone(),
            symbols: Vec::new(),
        })
    }
}

impl Retriever for PlaybookPhases {
    fn retrieve(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "contract requirements only".into(),
            files: self.files.clone(),
            symbols: Vec::new(),
        })
    }
}

/// Fail-closed drivers: the run itself is the implementer and there is no
/// repair loop, so a model phase reaching these is a wiring bug.
struct HostBlocked;

impl Implementer for HostBlocked {
    fn implement(
        &self,
        _packet: &AgentContextPacket,
    ) -> Result<CandidateCompletion, SupervisorError> {
        Err(SupervisorError::Agent(
            "the implementer is the run's own steps".into(),
        ))
    }
}

impl Planner for HostBlocked {
    fn plan(&self, _packet: &AgentContextPacket) -> Result<PlanResult, SupervisorError> {
        Err(SupervisorError::Agent(
            "a recovered run does not re-plan".into(),
        ))
    }
}

impl Explorer for HostBlocked {
    fn explore(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Err(SupervisorError::Agent(
            "a recovered run does not re-explore".into(),
        ))
    }
}

impl Retriever for HostBlocked {
    fn retrieve(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Err(SupervisorError::Agent(
            "a recovered run does not re-retrieve".into(),
        ))
    }
}

impl Strategist for HostBlocked {
    fn revise(
        &self,
        _packet: &AgentContextPacket,
        _gaps: &[GapNode],
        _attempts: u32,
    ) -> Result<StrategyRevision, SupervisorError> {
        Err(SupervisorError::Agent(
            "no strategist for a playbook run".into(),
        ))
    }
}

/// Replays the verification steps' real results; a check the run never
/// reached is `Skipped`, which the host gate does not count as passing
/// evidence for its requirement.
struct ReplayedChecks {
    results: CapturedChecks,
}

impl CheckRunner for ReplayedChecks {
    fn run(&self, check: &VerificationCheck) -> CheckResult {
        self.results
            .lock()
            .ok()
            .and_then(|map| map.get(&check.id).cloned())
            .unwrap_or_else(|| CheckResult {
                check_id: check.id.clone(),
                status: CheckStatus::Skipped,
                exit_code: None,
                duration_ms: 0,
                stdout_ref: None,
                stderr_ref: None,
                evidence_id: EvidenceId::new(),
                failure_summary: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{
        AgentStepFn, CommandStepFn, HumanWaitFn, RunContext, RunOutcome, execute_run, new_run_id,
    };
    use event_ledger::event::EventKind;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapid-wf-verified-{name}-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn step(key: &str, kind: NodeKind, depends_on: &[&str]) -> Step {
        Step {
            key: key.to_owned(),
            kind,
            label: key.to_owned(),
            depends_on: depends_on.iter().map(|d| (*d).to_owned()).collect(),
            budget_tokens: 0,
            max_attempts: 3,
            prompt: None,
            command: (kind == NodeKind::Verification).then(|| "true".to_owned()),
            watch: Vec::new(),
            timeout_secs: Some(2),
            question: None,
        }
    }

    fn diamond() -> PlaybookFile {
        PlaybookFile {
            name: "diamond".to_owned(),
            steps: vec![
                step("start", NodeKind::Task, &[]),
                step("left", NodeKind::Task, &["start"]),
                step("right", NodeKind::Task, &["start"]),
                step("check", NodeKind::Verification, &["left", "right"]),
            ],
        }
    }

    fn closures() -> (AgentStepFn, CommandStepFn, HumanWaitFn) {
        (
            Arc::new(|key, _task| Ok(format!("done: {key}"))),
            Arc::new(|_command, _timeout| Ok("ok".to_owned())),
            Arc::new(|_step, _state| Err("no human steps here".to_owned())),
        )
    }

    fn open(
        playbook: &PlaybookFile,
        state: &RunState,
        root: &Path,
        ledger_dir: &Path,
    ) -> (VerifiedRun, SessionId) {
        let ledger = EventLedger::open(ledger_dir.join("ledger.sqlite")).expect("ledger");
        let session = SessionId::new();
        let run = VerifiedRun::open(playbook, state, root, ledger, session).expect("open");
        (run, session)
    }

    /// Every `graph.node.state_changed` event in `session`, as
    /// `(step key or "goal", state)` in ledger order.
    fn transitions(
        ledger_dir: &Path,
        session: SessionId,
        nodes: &BTreeMap<String, NodeId>,
    ) -> Vec<(String, String)> {
        let ledger = EventLedger::open(ledger_dir.join("ledger.sqlite")).expect("ledger");
        let cancel = event_ledger::ledger::CancellationToken::new();
        let last = ledger.last_seq(session, &cancel).expect("last seq");
        let by_node: BTreeMap<String, &str> = nodes
            .iter()
            .map(|(key, id)| (id.to_string(), key.as_str()))
            .collect();
        (1..=last)
            .filter_map(|seq| ledger.get(session, seq, &cancel).ok())
            .filter(|event| event.kind() == EventKind::GraphNodeStateChanged)
            .map(|event| {
                let payload = event.payload();
                let node = payload["node_id"].as_str().unwrap_or("");
                (
                    by_node.get(node).map_or("goal", |key| key).to_owned(),
                    payload["state"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect()
    }

    fn kinds(ledger_dir: &Path, session: SessionId) -> Vec<EventKind> {
        let ledger = EventLedger::open(ledger_dir.join("ledger.sqlite")).expect("ledger");
        let cancel = event_ledger::ledger::CancellationToken::new();
        let last = ledger.last_seq(session, &cancel).expect("last seq");
        (1..=last)
            .filter_map(|seq| ledger.get(session, seq, &cancel).ok())
            .map(|event| event.kind())
            .collect()
    }

    fn context(root: &Path) -> RunContext<'_> {
        RunContext {
            root,
            trusted: true,
            max_parallel: 4,
            events: None,
        }
    }

    #[test]
    fn a_verified_run_records_every_transition_and_accepts_only_on_fresh_checks() {
        let root = scratch("accept");
        let playbook = diamond();
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        let (mut run, session) = open(&playbook, &state, &root, &root);
        let nodes = run.nodes().clone();
        let (agent, command, human) = closures();
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(outcome, RunOutcome::Verified);

        // The run state names the graph and records the acceptance.
        let record = state.orchestration.clone().expect("recorded");
        assert_eq!(record.mode, "verified");
        assert_eq!(record.session, session.to_string());
        assert_eq!(record.graph_id, run.graph_id().to_string());
        assert_eq!(record.state, "Accepted");
        assert!(record.accepted);

        // The ledger session holds the graph, the plan and every transition
        // — each step ran once, the check passed, the goal succeeded last.
        let kinds = kinds(&root, session);
        assert_eq!(kinds[0], EventKind::GraphCreated);
        assert_eq!(kinds[1], EventKind::GraphRevisionCommitted);
        let seen = transitions(&root, session, &nodes);
        let expect = |key: &str, state: &str| (key.to_owned(), state.to_owned());
        assert_eq!(seen[0], expect("start", "running"));
        assert_eq!(seen[1], expect("start", "succeeded"));
        let middle: Vec<_> = seen[2..6].to_vec();
        assert!(middle.contains(&expect("left", "running")));
        assert!(middle.contains(&expect("right", "running")));
        assert!(middle.contains(&expect("left", "succeeded")));
        assert!(middle.contains(&expect("right", "succeeded")));
        assert_eq!(seen[6], expect("check", "running"));
        assert_eq!(seen[7], expect("check", "succeeded"));
        assert_eq!(
            seen.len(),
            8,
            "the goal's completion is the acceptance record, not a node event"
        );
        // Acceptance is exactly one durable record, and it is the last
        // thing the run appends to the graph's stream (GVS-006).
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::OrchestrationTaskAccepted)
                .count(),
            1,
            "one acceptance record"
        );

        // Resuming the finished run: nothing runs again, the graph mirrors
        // the recorded states, and with no check run *in this invocation*
        // the supervisor refuses — finished is not verified.
        let (mut resumed, session2) = open(&playbook, &state, &root, &root);
        let nodes2 = resumed.nodes().clone();
        let (agent, command, human) = closures();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut resumed),
        );
        assert_eq!(
            outcome,
            RunOutcome::CompletedUnverified {
                unmet: vec!["check".to_owned()]
            }
        );
        let record = state.orchestration.clone().expect("recorded");
        assert_eq!(record.session, session2.to_string());
        assert!(!record.accepted);
        assert_eq!(record.state, "Blocked", "refuted, and out of rounds");
        let seen = transitions(&root, session2, &nodes2);
        assert_eq!(seen.len(), 4, "the mirror only: one per recorded step");
        assert!(seen.iter().all(|(_, state)| state == "succeeded"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_check_retries_on_the_graph_and_a_final_failure_is_never_accepted() {
        let root = scratch("retry");
        let mut playbook = PlaybookFile {
            name: "retry".to_owned(),
            steps: vec![step("build", NodeKind::Task, &[]), {
                let mut check = step("check", NodeKind::Verification, &["build"]);
                check.max_attempts = 2;
                check
            }],
        };
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("r.json"));
        let (mut run, session) = open(&playbook, &state, &root, &root);
        let nodes = run.nodes().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_command = Arc::clone(&calls);
        let command: CommandStepFn = Arc::new(move |_command, _timeout| {
            if calls_for_command.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("exit 1".to_owned())
            } else {
                Ok("ok".to_owned())
            }
        });
        let (agent, _, human) = closures();
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let seen = transitions(&root, session, &nodes);
        let check: Vec<&str> = seen
            .iter()
            .filter(|(key, _)| key == "check")
            .map(|(_, state)| state.as_str())
            .collect();
        assert_eq!(
            check,
            vec!["running", "failed", "pending", "running", "succeeded"],
            "the retry is a graph transition, not a private counter"
        );

        // A retrying *dependency* keeps its dependent waiting on the graph
        // and in the run state alike: `build` fails once, `check` is not
        // cancelled, and the run is accepted after the retry.
        let mut upstream = playbook.clone();
        upstream.steps[0].max_attempts = 2;
        upstream.steps[1].max_attempts = 1;
        let mut state = RunState::new(new_run_id(), &upstream, Path::new("u.json"));
        let (mut run, session) = open(&upstream, &state, &root, &root);
        let nodes = run.nodes().clone();
        let builds = Arc::new(AtomicUsize::new(0));
        let builds_for_agent = Arc::clone(&builds);
        let agent: AgentStepFn = Arc::new(move |_key, _task| {
            if builds_for_agent.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("transient".to_owned())
            } else {
                Ok("built".to_owned())
            }
        });
        let (_, command, human) = closures();
        let outcome = execute_run(
            &upstream,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(outcome, RunOutcome::Verified);
        let seen = transitions(&root, session, &nodes);
        assert!(
            !seen
                .iter()
                .any(|(key, state)| key == "check" && state == "cancelled")
        );

        // With one attempt the failure is final: the run stops before any
        // conclusion, the graph says `failed`, and nothing is accepted.
        playbook.steps[1].max_attempts = 1;
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("r.json"));
        let (mut run, session) = open(&playbook, &state, &root, &root);
        let nodes = run.nodes().clone();
        let command: CommandStepFn = Arc::new(|_command, _timeout| Err("exit 1".to_owned()));
        let (agent, _, human) = closures();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(
            outcome,
            RunOutcome::Failed {
                key: "check".to_owned(),
                reason: "exit 1".to_owned()
            }
        );
        let record = state.orchestration.clone().expect("recorded");
        assert!(!record.accepted);
        assert_eq!(record.state, "Implementing", "never concluded");
        let seen = transitions(&root, session, &nodes);
        assert_eq!(
            seen.last().cloned(),
            Some(("check".to_owned(), "failed".to_owned()))
        );
        assert!(!seen.iter().any(|(key, _)| key == "goal"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_human_step_waits_on_the_graph_and_the_approved_resume_is_accepted() {
        let root = scratch("human");
        let playbook = PlaybookFile {
            name: "gate".to_owned(),
            steps: vec![
                step("build", NodeKind::Task, &[]),
                step("gate", NodeKind::Approval, &["build"]),
                step("verify", NodeKind::Verification, &["gate"]),
            ],
        };
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("g.json"));
        let (mut run, session) = open(&playbook, &state, &root, &root);
        let nodes = run.nodes().clone();
        let (agent, command, _) = closures();
        let human: HumanWaitFn = Arc::new(|step, _state| Ok(format!("wait-{}", step.key)));
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(
            outcome,
            RunOutcome::Paused {
                key: "gate".to_owned(),
                wait_token: "wait-gate".to_owned()
            }
        );
        let seen = transitions(&root, session, &nodes);
        assert_eq!(
            seen.last().cloned(),
            Some(("gate".to_owned(), "waiting".to_owned()))
        );
        assert!(!state.orchestration.as_ref().expect("recorded").accepted);

        // The human approves (the resolve command's transition), and the
        // resumed run mirrors that before running what is now ready.
        state.steps.insert("gate".into(), StepState::Succeeded);
        state.paused_on = None;
        let (mut resumed, session2) = open(&playbook, &state, &root, &root);
        let nodes2 = resumed.nodes().clone();
        let (agent, command, _) = closures();
        let human: HumanWaitFn = Arc::new(|_step, _state| Err("not again".to_owned()));
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut resumed),
        );
        assert_eq!(outcome, RunOutcome::Verified);
        let seen = transitions(&root, session2, &nodes2);
        let expect = |key: &str, state: &str| (key.to_owned(), state.to_owned());
        assert_eq!(
            seen,
            vec![
                expect("build", "succeeded"),
                expect("gate", "succeeded"),
                expect("verify", "running"),
                expect("verify", "succeeded"),
            ],
            "the goal's completion rides the acceptance record, not a node event"
        );
        assert_eq!(
            kinds(&root, session2)
                .iter()
                .filter(|k| **k == EventKind::OrchestrationTaskAccepted)
                .count(),
            1,
            "one acceptance record"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_denied_human_step_resumed_verified_re_asks_rather_than_wedging() {
        // The state `rapid run --resolve <id> deny` leaves: the gate is
        // `Failed{u32::MAX}` with the counter set to match (so the run loop
        // and the graph mirror agree it is final). A `--resume
        // --orchestration verified` must not disagree with itself — the
        // regression was `mirror` reading only `state.attempts` (unset →
        // "retryable") while the loop read the `Failed` count ("final"),
        // producing `graph and run state disagree`.
        let root = scratch("denied");
        let playbook = PlaybookFile {
            name: "gate".to_owned(),
            steps: vec![
                step("gate", NodeKind::Approval, &[]),
                step("other", NodeKind::Task, &[]),
                step("verify", NodeKind::Verification, &["other"]),
            ],
        };
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("g.json"));
        // The state a `--resolve deny` leaves: `Failed{u32::MAX}` in the
        // step, and — the regression — nothing in the `attempts` counter.
        // `mirror` must read the same `failed_for_good` rule the run loop
        // does (which folds in the `Failed` count), not the bare counter,
        // or the graph re-queues `gate` while the loop treats it as final
        // and `confirm_ready` reports `graph and run state disagree`.
        state.steps.insert(
            "gate".into(),
            StepState::Failed {
                reason: "denied by the operator".into(),
                attempts: u32::MAX,
            },
        );
        let (mut run, _session) = open(&playbook, &state, &root, &root);
        let (agent, command, human) = closures();
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        // A denied gate fails the run (the operator said no) — but through
        // `RunOutcome::Failed`, never a self-disagreement.
        assert_eq!(
            outcome,
            RunOutcome::Failed {
                key: "gate".to_owned(),
                reason: "denied by the operator".to_owned(),
            },
            "must not wedge on `graph and run state disagree`"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resumed_verified_run_recovers_its_supervisor_as_paused() {
        // GVS-008: the run-state file now carries the supervisor snapshot,
        // so a later invocation reduces it back instead of forgetting what
        // the last one established — and it comes back *paused*, because an
        // interrupted run may have left effects nobody has settled.
        let root = scratch("recovered");
        let playbook = diamond();
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        assert_eq!(
            VerifiedRun::recovered_state(&state).expect("no error"),
            None,
            "a fresh run has nothing to recover"
        );

        let (mut run, _session) = open(&playbook, &state, &root, &root);
        let (agent, command, human) = closures();
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert_eq!(outcome, RunOutcome::Verified);

        // The accepted run recovers as accepted — a finished run must not be
        // reopened by a restart.
        assert_eq!(
            VerifiedRun::recovered_state(&state).expect("no error"),
            Some(OrchestrationState::Accepted)
        );

        // A run interrupted mid-flight recovers paused instead.
        let mut midway = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        let (run, _session) = open(&playbook, &midway, &root, &root);
        midway.orchestration = Some(run.record());
        assert_eq!(
            run.record().state,
            "Implementing",
            "the run was mid-flight when it was recorded"
        );
        assert_eq!(
            VerifiedRun::recovered_state(&midway).expect("no error"),
            Some(OrchestrationState::Paused),
            "an interrupted run comes back paused, not running"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_snapshot_does_not_make_the_whole_run_state_unloadable() {
        // The snapshot is a large structure inside the run-state file. If a
        // binary that cannot decode it failed the whole `RunState`, a single
        // schema change would brick every existing run — so it is read
        // leniently and the run stays resumable from its step records.
        let root = scratch("lenient");
        let playbook = diamond();
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        state.steps.insert("start".into(), StepState::Succeeded);
        state.orchestration = Some(OrchestrationRecord {
            mode: "verified".into(),
            session: "s".into(),
            graph_id: "g".into(),
            task_id: "t".into(),
            state: "Implementing".into(),
            accepted: false,
            snapshot: None,
        });
        crate::workflow::save_run(&root, &state).expect("save");

        // Corrupt just the snapshot, as a future schema change would.
        let path = root
            .join(".rapidlm")
            .join("runs")
            .join(format!("{}.json", state.run_id));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["orchestration"]["snapshot"] = serde_json::json!({"from": "a newer binary"});
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let reloaded = crate::workflow::load_run(&root, &playbook, &state.run_id)
            .expect("the run state still loads");
        assert_eq!(
            reloaded.steps.get("start"),
            Some(&StepState::Succeeded),
            "the run's own progress survives an undecodable snapshot"
        );
        assert!(
            reloaded
                .orchestration
                .as_ref()
                .expect("the record itself survives")
                .snapshot
                .is_none(),
            "the unreadable snapshot is simply absent"
        );
        assert_eq!(
            VerifiedRun::recovered_state(&reloaded).expect("no error"),
            None,
            "and there is nothing to recover from it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_playbook_that_cannot_be_verified_is_refused_before_anything_runs() {
        let root = scratch("refuse");
        let ledger = EventLedger::open(root.join("ledger.sqlite")).expect("ledger");
        let no_checks = PlaybookFile {
            name: "tasks-only".to_owned(),
            steps: vec![step("a", NodeKind::Task, &[])],
        };
        let state = RunState::new(new_run_id(), &no_checks, Path::new("n.json"));
        let err = VerifiedRun::open(&no_checks, &state, &root, ledger.clone(), SessionId::new())
            .err()
            .expect("refused");
        assert!(
            matches!(err, VerifiedRunError::NoVerificationSteps),
            "{err}"
        );

        let mut oversized = diamond();
        oversized.steps[3].command = Some("x".repeat(MAX_COMMAND_BYTES + 1));
        let state = RunState::new(new_run_id(), &oversized, Path::new("o.json"));
        let err = VerifiedRun::open(&oversized, &state, &root, ledger, SessionId::new())
            .err()
            .expect("refused");
        assert!(
            matches!(&err, VerifiedRunError::CommandTooLong { key, .. } if key == "check"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_ledger_that_stops_taking_appends_ends_the_run_with_nothing_accepted() {
        let root = scratch("ledger-gone");
        let ledger_dir = root.join("ledger");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        let playbook = diamond();
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        let (mut run, _session) = open(&playbook, &state, &root, &ledger_dir);
        // The ledger connects per call: with its directory gone the first
        // transition cannot be appended, so the step never starts.
        std::fs::remove_dir_all(&ledger_dir).unwrap();
        let (agent, command, human) = closures();
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        match outcome {
            RunOutcome::OrchestrationFailed { reason } => {
                assert!(reason.starts_with("graph: ledger:"), "{reason}");
            }
            other => panic!("expected an orchestration failure, got {other:?}"),
        }
        assert_eq!(state.steps.get("start"), Some(&StepState::Pending));
        let record = state.orchestration.expect("recorded at open");
        assert!(!record.accepted);

        // Lost *while a step runs*: the step's outcome is a fact that
        // happened, so the run state records it before the refusal ends the
        // run — a resume must not repeat the step because the graph could
        // not be told.
        std::fs::create_dir_all(&ledger_dir).unwrap();
        let mut state = RunState::new(new_run_id(), &playbook, Path::new("d.json"));
        let (mut run, _session) = open(&playbook, &state, &root, &ledger_dir);
        let ledger_to_remove = ledger_dir.clone();
        let agent: AgentStepFn = Arc::new(move |key, _task| {
            let _ = std::fs::remove_dir_all(&ledger_to_remove);
            Ok(format!("done: {key}"))
        });
        let (_, command, human) = closures();
        let outcome = execute_run(
            &playbook,
            &mut state,
            &context(&root),
            agent,
            command,
            human,
            &cancel,
            Some(&mut run),
        );
        assert!(
            matches!(outcome, RunOutcome::OrchestrationFailed { .. }),
            "{outcome:?}"
        );
        assert_eq!(state.steps.get("start"), Some(&StepState::Succeeded));
        assert_eq!(
            state.results.get("start").map(String::as_str),
            Some("done: start")
        );
        assert_eq!(
            state.steps.get("left"),
            Some(&StepState::Pending),
            "never started"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
