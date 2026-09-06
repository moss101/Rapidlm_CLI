//! Worker completion claims verified by the host supervisor (`goal claim`).
//!
//! Runs the verified-orchestration acceptance path over the active goal: the
//! operator submits a structured claim backed by deterministic check commands,
//! the host executes the checks for real, appends a ledger event per run, and
//! only the host supervisor can accept. Model-backed planner/implementer/
//! verifier drivers do not exist yet, so every injected driver is host-owned
//! and fails closed if a phase would need a model decision.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_runtime::{
    AcceptanceCriterion, AcceptancePolicy, AgentContextPacket, CandidateCompletion, CheckResult,
    CheckRunner, CheckStatus, CancellationToken, CompletionClaim, DiscoveryResult, EvidenceKind,
    EvidenceLedgerRef, EvidenceNode, EvidenceProducer, EvidenceSpec, EvidenceSource, EvidenceStatus,
    EvidenceTrust, Explorer, GapNode, GoalSnapshot, Implementer, MAX_ID_BYTES,
    OrchestrationBudget, OrchestrationEvidenceKind, OrchestrationState, Planner, PlanResult,
    RequirementClaim, RequirementClaimStatus, RequirementNode, Retriever, StrategyRevision,
    Strategist, Supervisor, SupervisorDrivers, SupervisorError, TaskContract, TaskComplexity,
    TransitionError, VerificationCheck, VerificationPolicy, Verdict, WorkspaceIdentity,
    WorkspacePolicy, TEST_PASSED,
};
use event_ledger::event::{ActorKind, ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, CancellationToken as LedgerCancel, EventLedger};
use protocol::{ArtifactId, EventId, EvidenceId, ProjectId, SessionId, TraceId};

use crate::goal_host::GoalHost;

/// Maximum accepted checks per claim.
pub const MAX_CLAIM_CHECKS: usize = 16;

/// Maximum UTF-8 bytes in a claim summary or check command.
pub const MAX_CLAIM_TEXT_BYTES: usize = 512;

/// Claim timeout bounds (seconds).
pub const MIN_TIMEOUT_SECS: u64 = 1;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Cap on captured check output per stream, hashed as the evidence source.
const MAX_CHECK_OUTPUT_BYTES: u64 = 64 * 1024;

/// One authorized deterministic check: requirement id plus command argv text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckSpec {
    pub requirement_id: String,
    pub command: String,
}

/// Parsed `goal claim` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalClaim {
    summary: String,
    checks: Vec<CheckSpec>,
    timeout: Duration,
}

impl GoalClaim {
    pub fn new(
        summary: impl Into<String>,
        mut checks: Vec<CheckSpec>,
        timeout_secs: u64,
    ) -> Result<Self, GoalClaimError> {
        let summary = summary.into();
        if summary.is_empty() || summary.len() > MAX_CLAIM_TEXT_BYTES {
            return Err(GoalClaimError::InvalidSummary);
        }
        if checks.is_empty() {
            return Err(GoalClaimError::NoChecks);
        }
        if checks.len() > MAX_CLAIM_CHECKS {
            return Err(GoalClaimError::TooManyChecks);
        }
        checks.sort_by(|a, b| a.requirement_id.cmp(&b.requirement_id));
        for check in &checks {
            if check.requirement_id.is_empty() || check.requirement_id.len() > MAX_CLAIM_TEXT_BYTES
            {
                return Err(GoalClaimError::InvalidRequirement);
            }
            if check.command.is_empty() || check.command.len() > MAX_CLAIM_TEXT_BYTES {
                return Err(GoalClaimError::InvalidCommand);
            }
        }
        if !checks.windows(2).all(|w| w[0].requirement_id != w[1].requirement_id) {
            return Err(GoalClaimError::DuplicateRequirement);
        }
        if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(GoalClaimError::InvalidTimeout);
        }
        Ok(Self {
            summary,
            checks,
            timeout: Duration::from_secs(timeout_secs),
        })
    }
}

/// Typed claim failure. Display never echoes claim text or command text.
#[derive(Debug, Eq, PartialEq)]
pub enum GoalClaimError {
    NoGoal,
    NoChecks,
    TooManyChecks,
    InvalidSummary,
    InvalidRequirement,
    InvalidCommand,
    InvalidTimeout,
    DuplicateRequirement,
    UnknownRequirement,
    InvalidCriterionId,
    CheckSpawn,
    Ledger,
    Cancelled,
    Supervisor(SupervisorError),
}

impl fmt::Display for GoalClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGoal => f.write_str("no active goal to claim"),
            Self::NoChecks => f.write_str("a claim requires at least one --check"),
            Self::TooManyChecks => write!(f, "a claim accepts at most {MAX_CLAIM_CHECKS} checks"),
            Self::InvalidSummary => f.write_str("claim summary is empty or exceeds bound"),
            Self::InvalidRequirement => f.write_str("check requirement id is empty or exceeds bound"),
            Self::InvalidCommand => f.write_str("check command is empty or exceeds bound"),
            Self::InvalidTimeout => {
                write!(
                    f,
                    "timeout must be {MIN_TIMEOUT_SECS}..={MAX_TIMEOUT_SECS} seconds"
                )
            }
            Self::DuplicateRequirement => f.write_str("duplicate check for one requirement"),
            Self::UnknownRequirement => f.write_str("check names a criterion the goal does not have"),
            Self::InvalidCriterionId => {
                f.write_str("a goal criterion id cannot form a valid contract id")
            }
            Self::CheckSpawn => f.write_str("check command could not be spawned"),
            Self::Ledger => f.write_str("ledger audit trail unavailable"),
            Self::Cancelled => f.write_str("claim cancelled"),
            Self::Supervisor(err) => write!(f, "{err}"),
        }
    }
}

impl Error for GoalClaimError {}

/// Real result of one deterministic check run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckOutcome {
    pub requirement_id: String,
    pub passed: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// What the host decided.
#[derive(Clone, Debug)]
pub struct ClaimOutcome {
    pub accepted: bool,
    pub verdict: Verdict,
    pub checks: Vec<CheckOutcome>,
    pub evidence_recorded: usize,
    /// Ledger citations recorded for the check runs, resolvable via
    /// [`EventLedger::get`].
    pub citations: Vec<EvidenceLedgerRef>,
}

struct CheckRun {
    passed: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    duration_ms: u64,
    output: Vec<u8>,
}

/// Execute one operator-authorized command directly (no shell), bounded by a
/// poll deadline. Output is captured and hashed; nothing is echoed.
///
/// Stdout/stderr are drained on background threads for the whole wait, not
/// read afterward: a check command that writes more than the OS pipe buffer
/// (commonly 16-64 KiB) before exiting would otherwise block in `write(2)`
/// once that buffer fills, so it would never reach the `try_wait` loop's
/// success case and get misreported as timed out even though it had already
/// finished — a real risk for anything as ordinary as a verbose test run.
fn run_check_command(spec: &CheckSpec, timeout: Duration) -> Result<CheckRun, GoalClaimError> {
    let mut parts = spec.command.split_whitespace();
    let Some(program) = parts.next() else {
        return Err(GoalClaimError::InvalidCommand);
    };
    let mut command = Command::new(program);
    command.args(parts);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| GoalClaimError::CheckSpawn)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader =
        stdout_pipe.map(|pipe| std::thread::spawn(move || drain_capped(pipe, MAX_CHECK_OUTPUT_BYTES)));
    let stderr_reader =
        stderr_pipe.map(|pipe| std::thread::spawn(move || drain_capped(pipe, MAX_CHECK_OUTPUT_BYTES)));
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                timed_out = true;
                let _ = child.kill();
                break child
                    .wait()
                    .map_err(|_| GoalClaimError::CheckSpawn)?;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return Err(GoalClaimError::CheckSpawn),
        }
    };
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut output = Vec::new();
    if let Some(reader) = stdout_reader {
        output.extend(reader.join().unwrap_or_default());
    }
    if let Some(reader) = stderr_reader {
        output.extend(reader.join().unwrap_or_default());
    }
    Ok(CheckRun {
        passed: !timed_out && status.success(),
        timed_out,
        exit_code: status.code(),
        duration_ms,
        output,
    })
}

/// Reads `pipe` to EOF, keeping only the first `cap` bytes. Never stops
/// early on overflow: draining must continue for the whole stream or the
/// child could block on a full pipe exactly as before, just past `cap`
/// instead of past the (much smaller) OS pipe buffer.
fn drain_capped<R: Read>(mut pipe: R, cap: u64) -> Vec<u8> {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                let take = room.min(n);
                out.extend_from_slice(&buf[..take]);
            }
        }
    }
    out
}

/// Host-owned phase drivers. Discovery/planning are derived from the goal
/// contract itself; no model is consulted.
struct HostPhases {
    steps: Vec<String>,
    files: Vec<String>,
}

impl Planner for HostPhases {
    fn plan(&self, _packet: &AgentContextPacket) -> Result<PlanResult, SupervisorError> {
        Ok(PlanResult {
            summary: "host plan derived from goal criteria".into(),
            steps: self.steps.clone(),
        })
    }
}

impl Explorer for HostPhases {
    fn explore(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "host discovery: bounded project listing".into(),
            files: self.files.clone(),
            symbols: Vec::new(),
        })
    }
}

impl Retriever for HostPhases {
    fn retrieve(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "host retrieval: contract requirements only".into(),
            files: self.files.clone(),
            symbols: Vec::new(),
        })
    }
}

/// Fail-closed drivers: the claim path is host-driven, so a model phase
/// reaching these is a wiring bug, not a prompt problem.
struct HostBlocked;

impl Implementer for HostBlocked {
    fn implement(
        &self,
        _packet: &AgentContextPacket,
    ) -> Result<CandidateCompletion, SupervisorError> {
        Err(SupervisorError::Agent(
            "implementer is host-driven for goal claims".into(),
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
            "strategist is not available for goal claims".into(),
        ))
    }
}

/// Replays the real check results captured at execution time. The supervisor
/// audit trail must reflect the single real run, never a second execution.
struct CachedChecks {
    results: BTreeMap<String, CheckResult>,
}

impl CheckRunner for CachedChecks {
    fn run(&self, check: &VerificationCheck) -> CheckResult {
        self.results
            .get(&check.id)
            .cloned()
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

/// Bounded top-level project listing for the discovery phase.
fn bounded_project_listing() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(".") else {
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

/// Contract ids are namespaced by goal criterion id: requirements must be
/// `REQ-*` and acceptance criteria `AC-*` per `TaskContract::validate`.
fn req_id(criterion_id: &str) -> String {
    format!("REQ-{criterion_id}")
}

fn ac_id(criterion_id: &str) -> String {
    format!("AC-{criterion_id}")
}

/// Fail closed before any run when a criterion id could never appear in a
/// valid contract (bound and charset per `TaskContract::validate`).
fn validate_criterion_ids(snapshot: &GoalSnapshot) -> Result<(), GoalClaimError> {
    for criterion in snapshot.completion_criteria() {
        let id = criterion.id();
        let charset_ok = id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if id.is_empty() || id.len() + "REQ-".len() > MAX_ID_BYTES || !charset_ok {
            return Err(GoalClaimError::InvalidCriterionId);
        }
    }
    Ok(())
}

fn build_contract(snapshot: &GoalSnapshot) -> Result<TaskContract, SupervisorError> {
    let requirements: Vec<RequirementNode> = snapshot
        .completion_criteria()
        .iter()
        .map(|criterion| RequirementNode {
            id: req_id(criterion.id()),
            description: criterion.text().to_owned(),
            source: "goal-criterion".into(),
            priority: agent_runtime::RequirementPriority::Normal,
            mandatory: true,
            depends_on: Vec::new(),
            acceptance_criteria: vec![ac_id(criterion.id())],
        })
        .collect();
    let acceptance_criteria: Vec<AcceptanceCriterion> = snapshot
        .completion_criteria()
        .iter()
        .map(|criterion| AcceptanceCriterion {
            id: ac_id(criterion.id()),
            text: criterion.text().to_owned(),
        })
        .collect();
    let contract = TaskContract {
        id: snapshot.id(),
        parent_task_id: None,
        objective: snapshot.statement().to_owned(),
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
            max_wall_clock_ms: 600_000,
            max_tool_executions: 0,
        },
        context_budget: 0,
        workspace_policy: WorkspacePolicy::ReadOnly,
        complexity: TaskComplexity::Standard,
        metadata: std::collections::BTreeSet::new(),
    };
    contract
        .validate()
        .map_err(SupervisorError::InvalidContract)?;
    Ok(contract)
}

fn workspace_identity(snapshot: &GoalSnapshot) -> WorkspaceIdentity {
    let json = serde_json::to_string(snapshot).unwrap_or_default();
    WorkspaceIdentity {
        hash: ArtifactId::from_bytes(json.as_bytes()).to_string(),
    }
}

fn orchestration_kind(goal_kind: EvidenceKind) -> OrchestrationEvidenceKind {
    match goal_kind {
        EvidenceKind::Test => OrchestrationEvidenceKind::Test,
        EvidenceKind::Build => OrchestrationEvidenceKind::Build,
        EvidenceKind::Lint => OrchestrationEvidenceKind::Lint,
        EvidenceKind::Scan => OrchestrationEvidenceKind::Diagnostic,
        EvidenceKind::Diff => OrchestrationEvidenceKind::Diff,
        EvidenceKind::RuntimeObservation => OrchestrationEvidenceKind::Runtime,
        EvidenceKind::Command => OrchestrationEvidenceKind::Tool,
        EvidenceKind::Source => OrchestrationEvidenceKind::File,
        _ => OrchestrationEvidenceKind::Tool,
    }
}

/// Kinds the goal gate requires for `requirement_id`, as typed kinds. Falls
/// back to `command` when a criterion has no (or only unknown) requirements,
/// so a passing real run is still recorded.
fn required_kinds_for(snapshot: &GoalSnapshot, requirement_id: &str) -> Vec<EvidenceKind> {
    let mut kinds: Vec<EvidenceKind> = snapshot
        .evidence_requirements()
        .iter()
        .filter(|req| req.criterion_id() == requirement_id)
        .flat_map(|req| req.kinds().iter().map(|k| k.to_owned()))
        .filter_map(|k| k.parse().ok())
        .collect();
    kinds.sort();
    kinds.dedup();
    if kinds.is_empty() {
        kinds.push(EvidenceKind::Command);
    }
    kinds
}

/// Record one check observation into the goal evidence store: one record per
/// required kind, System producer, citing the ledger event appended for the
/// real run.
///
/// Persists immediately, under `GoalHost::update_evidence`'s lock — not
/// accumulated in `host`'s own in-memory store across the whole claim and
/// saved once at the end. `run_claim`'s own per-check loop can span many
/// minutes across several checks (each running a real, potentially slow
/// external command); batching every check's evidence into one save at the
/// very end would mean any *other* evidence writer's own commit during
/// that window — another `rapid goal evidence record`, a second concurrent
/// `rapid goal claim` — gets silently erased when this claim finally
/// flushes its own stale, pre-those-changes copy. Locking per check keeps
/// each lock hold to milliseconds (the check's own command has already
/// finished by the time this is called) while still reflecting every
/// writer's committed work.
#[allow(clippy::too_many_arguments)]
fn record_goal_evidence(
    host: &mut GoalHost,
    evidence_path: &Path,
    snapshot: &GoalSnapshot,
    check: &CheckSpec,
    run: &CheckRun,
    citation: &EvidenceLedgerRef,
) -> Result<usize, GoalClaimError> {
    let status = if run.passed {
        EvidenceStatus::Passed
    } else {
        EvidenceStatus::Failed
    };
    let source = EvidenceSource::new(ArtifactId::from_bytes(&run.output))
        .with_locator(check.command.clone())
        .map_err(|_| GoalClaimError::InvalidCommand)?;
    let kinds = required_kinds_for(snapshot, &check.requirement_id);
    let mut specs = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let assertion = if kind == EvidenceKind::Test {
            TEST_PASSED.to_owned()
        } else {
            "deterministic check".to_owned()
        };
        let spec = EvidenceSpec::new(
            EvidenceId::new(),
            snapshot.id(),
            kind,
            assertion,
            EvidenceProducer::System,
            source.clone(),
            status,
            check.requirement_id.clone(),
        )
        .map_err(|_| GoalClaimError::InvalidRequirement)?
        .with_criterion_id(check.requirement_id.clone())
        .map_err(|_| GoalClaimError::InvalidRequirement)?
        .with_command(check.command.clone())
        .map_err(|_| GoalClaimError::InvalidCommand)?
        .with_ledger_ref(citation.clone());
        specs.push(spec);
    }
    let recorded = specs.len();
    host.update_evidence(evidence_path, |host| -> Result<(), agent_runtime::EvidenceError> {
        for spec in specs {
            host.record_evidence(spec)?;
        }
        Ok(())
    })
    .map_err(|err| {
        match &err {
            crate::goal_host::GoalTransactionError::Persist(persist_err) => {
                eprintln!("{persist_err}");
            }
            crate::goal_host::GoalTransactionError::Mutate(mutate_err) => {
                eprintln!("{mutate_err}");
            }
        }
        GoalClaimError::Ledger
    })?;
    Ok(recorded)
}

/// Run the verified acceptance path: real checks, ledger-cited evidence,
/// host-gated verdict. On success the goal's criteria are satisfied by real
/// observations and `goal verify` completes.
pub fn run_claim(
    host: &mut GoalHost,
    evidence_path: &Path,
    ledger: &EventLedger,
    claim: GoalClaim,
    cancel: &CancellationToken,
) -> Result<ClaimOutcome, GoalClaimError> {
    let snapshot = host.snapshot().cloned().ok_or(GoalClaimError::NoGoal)?;
    validate_criterion_ids(&snapshot)?;
    for check in &claim.checks {
        if !snapshot
            .completion_criteria()
            .iter()
            .any(|criterion| criterion.id() == check.requirement_id)
        {
            return Err(GoalClaimError::UnknownRequirement);
        }
    }

    // Run every check once, for real, before the supervisor exists: the audit
    // runner replays these results, it never executes a second time.
    let ledger_cancel = LedgerCancel::new();
    let session = SessionId::new();
    ledger
        .create_session(session, ProjectId::new(), &ledger_cancel)
        .map_err(|_| GoalClaimError::Ledger)?;

    let mut cached: BTreeMap<String, CheckResult> = BTreeMap::new();
    let mut passed_nodes: Vec<(EvidenceId, EvidenceNode)> = Vec::new();
    let mut claims: Vec<RequirementClaim> = Vec::new();
    let mut outcomes: Vec<CheckOutcome> = Vec::new();
    let mut citations: Vec<EvidenceLedgerRef> = Vec::new();
    let mut evidence_recorded = 0;

    for check in &claim.checks {
        if cancel.is_cancelled() {
            return Err(GoalClaimError::Cancelled);
        }
        let run = run_check_command(check, claim.timeout)?;
        let envelope = ledger
            .append(
                session,
                ActorRef::new(ActorKind::System, &EventId::new().to_string())
                    .map_err(|_| GoalClaimError::Ledger)?,
                EventKind::ToolCompleted,
                serde_json::json!({"phase": "goal_claim_check"}),
                &AppendOptions {
                    redaction: protocol::RedactionClass::Project,
                    trace_id: TraceId::new(),
                    expected_seq: None,
                },
                &ledger_cancel,
            )
            .map_err(|_| GoalClaimError::Ledger)?;
        let citation =
            EvidenceLedgerRef::new(session, envelope.event_id().to_string(), envelope.seq())
                .map_err(|_| GoalClaimError::Ledger)?;

        let node_id = EvidenceId::new();
        let recorded =
            record_goal_evidence(host, evidence_path, &snapshot, check, &run, &citation)?;
        evidence_recorded += recorded;
        citations.push(citation);

        if run.passed {
            let kinds = required_kinds_for(&snapshot, &check.requirement_id);
            let node = EvidenceNode {
                id: node_id,
                task_id: snapshot.id(),
                requirement_ids: vec![req_id(&check.requirement_id)],
                producer: "host".into(),
                kind: orchestration_kind(kinds[0]),
                source: check.requirement_id.clone(),
                timestamp: envelope.seq(),
                artifact_ref: None,
                command_ref: Some(check.command.clone()),
                workspace_ref: None,
                content_hash: Some(ArtifactId::from_bytes(&run.output).to_string()),
                summary: "deterministic check passed".into(),
                structured_payload: None,
                trust_level: EvidenceTrust::Deterministic,
            };
            passed_nodes.push((node_id, node));
            claims.push(RequirementClaim {
                requirement_id: req_id(&check.requirement_id),
                claimed_status: RequirementClaimStatus::Satisfied,
                evidence_refs: vec![node_id],
                explanation: "deterministic check passed".into(),
            });
        } else {
            claims.push(RequirementClaim {
                requirement_id: req_id(&check.requirement_id),
                claimed_status: RequirementClaimStatus::Unsatisfied,
                evidence_refs: Vec::new(),
                explanation: "deterministic check failed".into(),
            });
        }

        let status = if run.passed {
            CheckStatus::Passed
        } else if run.timed_out {
            CheckStatus::Timeout
        } else {
            CheckStatus::Failed
        };
        cached.insert(
            check.requirement_id.clone(),
            CheckResult {
                check_id: check.requirement_id.clone(),
                status,
                exit_code: run.exit_code,
                duration_ms: run.duration_ms,
                stdout_ref: None,
                stderr_ref: None,
                evidence_id: node_id,
                failure_summary: (!run.passed).then(|| "check failed or timed out".into()),
            },
        );
        outcomes.push(CheckOutcome {
            requirement_id: check.requirement_id.clone(),
            passed: run.passed,
            timed_out: run.timed_out,
            exit_code: run.exit_code,
            duration_ms: run.duration_ms,
        });
    }

    let contract = build_contract(&snapshot).map_err(GoalClaimError::Supervisor)?;
    let steps: Vec<String> = snapshot
        .completion_criteria()
        .iter()
        .map(|c| c.text().to_owned())
        .collect();
    let files = bounded_project_listing();
    let drivers = SupervisorDrivers {
        planner: Box::new(HostPhases {
            steps: steps.clone(),
            files: files.clone(),
        }),
        explorer: Box::new(HostPhases {
            steps: Vec::new(),
            files: files.clone(),
        }),
        retriever: Box::new(HostPhases {
            steps: Vec::new(),
            files,
        }),
        implementer: Box::new(HostBlocked),
        verifiers: Vec::new(),
        strategist: Box::new(HostBlocked),
        checks: Box::new(CachedChecks { results: cached }),
    };
    let mut supervisor = Supervisor::start(contract, workspace_identity(&snapshot), drivers)
        .map_err(GoalClaimError::Supervisor)?;

    // Host-owned phases only: stop before any phase that would need a model.
    loop {
        if cancel.is_cancelled() {
            return Err(GoalClaimError::Cancelled);
        }
        match supervisor.state() {
            OrchestrationState::Implementing => break,
            OrchestrationState::Contracting
            | OrchestrationState::Discovering
            | OrchestrationState::Planning
            | OrchestrationState::Retrieving
            | OrchestrationState::ReadyToImplement => {
                supervisor.advance().map_err(GoalClaimError::Supervisor)?;
            }
            _ => {
                return Err(GoalClaimError::Supervisor(SupervisorError::Transition(
                    TransitionError::InvalidTransition,
                )));
            }
        }
    }

    // Host records the real observations first; the claim cites them.
    let node_ids: Vec<EvidenceId> = passed_nodes
        .iter()
        .map(|(id, _)| *id)
        .collect();
    for (_, node) in &passed_nodes {
        supervisor
            .record_evidence(node.clone())
            .map_err(GoalClaimError::Supervisor)?;
    }

    let candidate = CandidateCompletion {
        task_id: snapshot.id(),
        summary: claim.summary.clone(),
        changed_artifacts: Vec::new(),
        requirement_claims: claims,
        evidence_refs: node_ids,
        checks_requested: claim
            .checks
            .iter()
            .map(|c| c.requirement_id.clone())
            .collect(),
        known_limitations: Vec::new(),
        unresolved_items: Vec::new(),
        completion_claim: CompletionClaim::Done,
    };
    supervisor
        .submit_candidate(candidate)
        .map_err(GoalClaimError::Supervisor)?;
    supervisor.run_checks().map_err(GoalClaimError::Supervisor)?;
    let verdict = supervisor.verify().map_err(GoalClaimError::Supervisor)?;
    let mut accepted = false;
    if supervisor.state() == OrchestrationState::Verified {
        supervisor.accept().map_err(GoalClaimError::Supervisor)?;
        accepted = true;
    }
    Ok(ClaimOutcome {
        accepted,
        verdict: verdict.verdict,
        checks: outcomes,
        evidence_recorded,
        citations,
    })
}

/// Resolve a citation against the ledger (test/CLI audit helper).
pub fn citation_resolves(
    ledger: &EventLedger,
    citation: &EvidenceLedgerRef,
) -> Result<bool, GoalClaimError> {
    let cancel = LedgerCancel::new();
    match ledger.get(citation.session_id(), citation.seq(), &cancel) {
        Ok(envelope) => Ok(envelope.event_id().to_string() == citation.event_id()),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::{Criterion, EvidenceRequirement, GoalActor, GoalBudget, GoalCommand, GoalSpec};
    use protocol::GoalId;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapid-goal-claim-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn host_with_requirements(requires: &[(&str, &str)]) -> GoalHost {
        let mut host = GoalHost::new();
        let criteria: Vec<Criterion> = requires
            .iter()
            .map(|(id, text)| Criterion::new(*id, *text).expect("criterion"))
            .collect();
        let requirements: Vec<EvidenceRequirement> = requires
            .iter()
            .map(|(id, kinds)| {
                EvidenceRequirement::new(
                    *id,
                    kinds.split(',').map(str::to_owned).collect(),
                )
                .expect("req")
            })
            .collect();
        let spec = GoalSpec::new(
            GoalId::new(),
            "ship auth",
            criteria,
            GoalBudget::new(None, None, None, None),
            requirements,
        )
        .expect("spec");
        host.apply(
            GoalCommand::Create(spec),
            &GoalActor::Human,
            &CancellationToken::new(),
        )
        .expect("create");
        host
    }

    fn claim(checks: Vec<(&str, &str)>) -> GoalClaim {
        let checks: Vec<CheckSpec> = checks
            .into_iter()
            .map(|(requirement_id, command)| CheckSpec {
                requirement_id: requirement_id.to_owned(),
                command: command.to_owned(),
            })
            .collect();
        GoalClaim::new("host claim", checks, 30).expect("claim")
    }

    #[test]
    fn passing_claim_completes_goal_and_cites_ledger() {
        let dir = scratch("pass");
        let ledger = EventLedger::open(dir.join("sessions.sqlite")).expect("ledger");
        let mut host = host_with_requirements(&[("c1", "test")]);

        let outcome = run_claim(
            &mut host,
            &dir.join(crate::goal_host::EVIDENCE_FILE),
            &ledger,
            claim(vec![("c1", "/bin/echo ok")]),
            &CancellationToken::new(),
        )
        .expect("claim runs");

        assert!(outcome.accepted);
        assert_eq!(outcome.verdict, Verdict::Verified);
        assert!(outcome.checks[0].passed);
        assert_eq!(outcome.checks[0].exit_code, Some(0));
        assert!(outcome.evidence_recorded >= 1);
        for citation in &outcome.citations {
            assert!(
                citation_resolves(&ledger, citation).expect("resolve"),
                "citations must resolve against the real ledger"
            );
        }
        // The real run satisfies the criterion: the gate completes.
        assert!(host.can_complete(&CancellationToken::new()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failing_check_refutes_but_records_failed_evidence() {
        let dir = scratch("fail");
        let ledger = EventLedger::open(dir.join("sessions.sqlite")).expect("ledger");
        let mut host = host_with_requirements(&[("c1", "test")]);

        let outcome = run_claim(
            &mut host,
            &dir.join(crate::goal_host::EVIDENCE_FILE),
            &ledger,
            claim(vec![("c1", "/usr/bin/false")]),
            &CancellationToken::new(),
        )
        .expect("claim runs");

        assert!(!outcome.accepted);
        assert_eq!(outcome.verdict, Verdict::Refuted);
        assert!(!outcome.checks[0].passed);
        let records = host.evidence().store().records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status(), EvidenceStatus::Failed);
        assert!(!host.can_complete(&CancellationToken::new()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unclaimed_requirement_refutes_even_when_checked_one_passes() {
        let dir = scratch("partial");
        let ledger = EventLedger::open(dir.join("sessions.sqlite")).expect("ledger");
        let mut host = host_with_requirements(&[("c1", "test"), ("c2", "build")]);

        let outcome = run_claim(
            &mut host,
            &dir.join(crate::goal_host::EVIDENCE_FILE),
            &ledger,
            claim(vec![("c1", "/bin/echo ok")]),
            &CancellationToken::new(),
        )
        .expect("claim runs");

        assert!(!outcome.accepted);
        assert_eq!(outcome.verdict, Verdict::Refuted);
        // The passing check is still real evidence for c1.
        assert!(host.evidence().store().records().iter().any(|record| {
            record.criterion_id() == Some("c1") && record.status() == EvidenceStatus::Passed
        }));
        assert!(!host.can_complete(&CancellationToken::new()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timed_out_check_is_a_failed_check() {
        let dir = scratch("timeout");
        let ledger = EventLedger::open(dir.join("sessions.sqlite")).expect("ledger");
        let mut host = host_with_requirements(&[("c1", "test")]);

        let slow = GoalClaim::new(
            "host claim",
            vec![CheckSpec {
                requirement_id: "c1".into(),
                command: "/bin/sleep 5".into(),
            }],
            1,
        )
        .expect("claim");
        let outcome = run_claim(
            &mut host,
            &dir.join(crate::goal_host::EVIDENCE_FILE),
            &ledger,
            slow,
            &CancellationToken::new(),
        )
            .expect("claim runs");

        assert!(!outcome.accepted);
        assert!(outcome.checks[0].timed_out);
        assert!(!outcome.checks[0].passed);
        assert!(!host.can_complete(&CancellationToken::new()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_requirement_is_rejected_before_any_run() {
        let ledger_dir = scratch("unknown-ledger");
        let ledger = EventLedger::open(ledger_dir.join("sessions.sqlite")).expect("ledger");
        let mut host = host_with_requirements(&[("c1", "test")]);
        let err = run_claim(
            &mut host,
            &ledger_dir.join(crate::goal_host::EVIDENCE_FILE),
            &ledger,
            claim(vec![("nope", "/bin/echo ok")]),
            &CancellationToken::new(),
        )
        .expect_err("unknown requirement");
        assert!(matches!(err, GoalClaimError::UnknownRequirement));
        assert!(host.evidence().store().is_empty());
        std::fs::remove_dir_all(&ledger_dir).ok();
    }

    #[test]
    fn claim_validation_bounds() {
        assert_eq!(
            GoalClaim::new("s", Vec::new(), 30).unwrap_err(),
            GoalClaimError::NoChecks
        );
        assert_eq!(
            GoalClaim::new("", vec![check("c1", "/bin/echo")], 30).unwrap_err(),
            GoalClaimError::InvalidSummary
        );
        assert_eq!(
            GoalClaim::new("s", vec![check("c1", "")], 30).unwrap_err(),
            GoalClaimError::InvalidCommand
        );
        assert_eq!(
            GoalClaim::new("s", vec![check("c1", "/bin/echo"), check("c1", "/bin/echo")], 30)
                .unwrap_err(),
            GoalClaimError::DuplicateRequirement
        );
        assert_eq!(
            GoalClaim::new("s", vec![check("c1", "/bin/echo")], 0).unwrap_err(),
            GoalClaimError::InvalidTimeout
        );
        assert_eq!(
            GoalClaim::new("s", vec![check("c1", "/bin/echo")], 601).unwrap_err(),
            GoalClaimError::InvalidTimeout
        );
    }

    fn check(requirement_id: &str, command: &str) -> CheckSpec {
        CheckSpec {
            requirement_id: requirement_id.to_owned(),
            command: command.to_owned(),
        }
    }

    /// dd's output (200 KiB) comfortably exceeds every common OS pipe buffer
    /// (typically 16-64 KiB). A check command must not be misreported as
    /// timed out just because it writes more than that before exiting.
    #[cfg(unix)]
    #[test]
    fn check_command_writing_past_the_pipe_buffer_does_not_time_out() {
        let spec = CheckSpec {
            requirement_id: "c1".into(),
            command: "/bin/dd if=/dev/zero bs=1024 count=200".into(),
        };
        let run = run_check_command(&spec, Duration::from_secs(5)).expect("run");
        assert!(
            !run.timed_out,
            "output past the pipe buffer must not deadlock the wait into a false timeout"
        );
        assert!(run.passed);
        assert!(
            run.output.len() >= MAX_CHECK_OUTPUT_BYTES as usize,
            "captured output should hit its cap, not come back empty/truncated-to-nothing: got {} bytes",
            run.output.len()
        );
    }
}
