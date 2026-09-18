//! Host supervisor. Models return typed results; only [`Supervisor::accept`]
//! can produce [`OrchestrationState::Accepted`].

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::orchestration::evidence::CompletionClaim;
#[cfg(test)]
use protocol::EvidenceId;
use protocol::{GoalId, ModelPolicyName, OrchestrationMode, RapidConfig};
use serde::{Deserialize, Serialize};

use crate::evidence::{
    EvidenceProducer, EvidenceSource, EvidenceSpec, EvidenceStatus, EvidenceStore,
};
use crate::orchestration::checks::{CheckResult, CheckRunner, CheckStatus, VerificationCheck};
use crate::orchestration::contract::{TaskContract, TaskContractError};
use crate::orchestration::events::{
    MemoryEventSink, OrchestrationEvent, OrchestrationEventKind, OrchestrationEventSink,
};
use crate::orchestration::evidence::{
    CandidateCompletion, EvidenceNode, RequirementClaimStatus, VerificationEdge,
    VerificationRelation, WorkspaceIdentity,
};
use crate::orchestration::gaps::{GapCategory, GapNode, GapSeverity, GapStatus, RepairDirective};
use crate::orchestration::policy::{
    AcceptancePolicy, AggregationPolicy, OrchestrationBudget, OrchestrationRole, RoleModelResolver,
    StagnationDetector, VerifierPanel, profile_allows_writes,
};
use crate::orchestration::state::{
    OrchestrationState, OrchestrationTransition, TransitionError, validate_transition,
};
use crate::orchestration::verification::{
    Attestation, RequirementVerification, RequirementVerificationStatus, Verdict,
    VerificationVerdict,
};
/// Bounded packet passed to a worker. Never includes another agent's transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentContextPacket {
    pub role: OrchestrationRole,
    pub task_id: GoalId,
    pub objective: String,
    pub selected_requirements: Vec<String>,
    pub evidence_summaries: Vec<String>,
    pub unresolved_gap_ids: Vec<String>,
    pub workspace_identity: WorkspaceIdentity,
    pub budget: OrchestrationBudget,
    pub instructions: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanResult {
    pub summary: String,
    pub steps: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResult {
    pub summary: String,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyRevision {
    pub summary: String,
    pub constraints: Vec<String>,
    pub resume_from: OrchestrationState,
}

pub trait Planner {
    fn plan(&self, packet: &AgentContextPacket) -> Result<PlanResult, SupervisorError>;
}

pub trait Explorer {
    fn explore(&self, packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError>;
}

pub trait Retriever {
    fn retrieve(&self, packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError>;
}

pub trait Implementer {
    fn implement(
        &self,
        packet: &AgentContextPacket,
    ) -> Result<CandidateCompletion, SupervisorError>;
}

pub trait Verifier {
    fn verify(
        &self,
        packet: &AgentContextPacket,
        candidate: &CandidateCompletion,
        checks: &[CheckResult],
        evidence: &[EvidenceNode],
        previous_gaps: &[GapNode],
    ) -> Result<VerificationVerdict, SupervisorError>;
}

pub trait Strategist {
    fn revise(
        &self,
        packet: &AgentContextPacket,
        gaps: &[GapNode],
        attempts: u32,
    ) -> Result<StrategyRevision, SupervisorError>;
}

/// Injected workers. The supervisor never constructs model drivers itself.
pub struct SupervisorDrivers {
    pub planner: Box<dyn Planner>,
    pub explorer: Box<dyn Explorer>,
    pub retriever: Box<dyn Retriever>,
    pub implementer: Box<dyn Implementer>,
    pub verifiers: Vec<Box<dyn Verifier>>,
    pub strategist: Box<dyn Strategist>,
    pub checks: Box<dyn CheckRunner>,
}

/// Serializable orchestration snapshot for resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrchestrationSnapshot {
    pub state: OrchestrationState,
    pub contract: TaskContract,
    pub workspace_identity: WorkspaceIdentity,
    pub current_identity: WorkspaceIdentity,
    pub round: u32,
    pub repair_rounds: u32,
    pub strategist_calls: u32,
    pub tokens_used: u64,
    pub evidence: Vec<EvidenceNode>,
    pub edges: Vec<VerificationEdge>,
    pub attestations: Vec<Attestation>,
    pub gaps: Vec<GapNode>,
    pub checks: Vec<CheckResult>,
    pub last_candidate: Option<CandidateCompletion>,
    pub last_verdict: Option<VerificationVerdict>,
    pub last_repair: Option<RepairDirective>,
    pub last_plan: Option<PlanResult>,
    pub last_discovery: Option<DiscoveryResult>,
}

/// What one acceptance asserts: the task, the verdict that justified it,
/// the candidate it accepts and the workspace the attestation spoke about.
/// [`Supervisor::acceptance_record`] produces it by validating;
/// [`Supervisor::apply_acceptance`] consumes it by reducing. A caller that
/// owns more than one projection appends it durably in between, so both
/// projections are derived from one record instead of racing several
/// writes (ADR 0021 §2, GVS-006).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AcceptanceRecord {
    pub task_id: GoalId,
    pub verdict: Verdict,
    /// Content digest of the accepted [`CandidateCompletion`] — the
    /// candidate's identity until GVS-004 introduces a `CandidateId`.
    pub candidate_digest: String,
    pub workspace_identity: WorkspaceIdentity,
}

/// Stable content digest of a candidate completion. Serialization of an
/// owned value cannot fail; an empty digest would still be stable, so the
/// fallback is inert rather than a panic.
fn candidate_digest(candidate: &CandidateCompletion) -> String {
    let bytes = serde_json::to_vec(candidate).unwrap_or_default();
    protocol::ArtifactId::from_bytes(&bytes).to_string()
}

/// Host-owned supervisor. Disabled unless constructed explicitly.
pub struct Supervisor {
    snapshot: OrchestrationSnapshot,
    events: MemoryEventSink,
    store: EvidenceStore,
    stagnation: StagnationDetector,
    resolver: RoleModelResolver,
    panel: VerifierPanel,
    extra_sink: Option<Box<dyn OrchestrationEventSink>>,
    drivers: SupervisorDrivers,
    clock: u64,
    /// Wall-clock start of this process's run, checked against
    /// `resource_budget.max_wall_clock_ms` in `check_budget`. Not part of
    /// `OrchestrationSnapshot` (which is meant to be resumable/serializable
    /// — `Instant` is neither): `resume()` restarts this window rather than
    /// resuming the original run's elapsed time, a narrower, deliberate
    /// limitation short of full cross-process wall-clock persistence.
    started_at: Instant,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SupervisorError {
    Disabled,
    InvalidContract(TaskContractError),
    Transition(TransitionError),
    MissingCandidate,
    MissingEvidence,
    StaleWorkspace,
    BudgetExceeded,
    InvalidOutput,
    VerifierUnavailable,
    WriteForbidden,
    Sink(String),
    Agent(String),
}

impl Supervisor {
    /// Composition-root entry: `None` unless experimental verified mode is on.
    pub fn from_config(
        config: &RapidConfig,
        identity: WorkspaceIdentity,
        contract: TaskContract,
        drivers: SupervisorDrivers,
    ) -> Result<Option<Self>, SupervisorError> {
        if config.orchestration.mode != OrchestrationMode::Verified {
            return Ok(None);
        }
        Ok(Some(Self::start(contract, identity, drivers)?))
    }

    pub fn start(
        contract: TaskContract,
        identity: WorkspaceIdentity,
        drivers: SupervisorDrivers,
    ) -> Result<Self, SupervisorError> {
        contract
            .validate()
            .map_err(SupervisorError::InvalidContract)?;
        // `VerifierPanel::for_policy` sizes its panel to `skeptic_count.max(1)`
        // independent assignments (e.g. `Unanimous` aggregation for
        // `TaskComplexity::Critical` requires 2 agreeing skeptics) — but
        // `verify()` only ever iterates the drivers actually injected here,
        // never the panel's own assignment count. Checking for merely
        // "zero verifiers supplied" let a caller under-provision (e.g. one
        // verifier for a `skeptic_count: 2` policy) and still start
        // successfully, silently satisfying a multi-skeptic requirement
        // with a single verdict once `aggregate()`'s `Unanimous` branch saw
        // no disagreement in a one-element list.
        if drivers.verifiers.len() < contract.verification_policy.skeptic_count as usize {
            return Err(SupervisorError::VerifierUnavailable);
        }
        let panel = VerifierPanel::for_policy(&contract.verification_policy);
        let mut supervisor = Self {
            snapshot: OrchestrationSnapshot {
                state: OrchestrationState::Created,
                contract,
                workspace_identity: identity.clone(),
                current_identity: identity,
                round: 0,
                repair_rounds: 0,
                strategist_calls: 0,
                tokens_used: 0,
                evidence: Vec::new(),
                edges: Vec::new(),
                attestations: Vec::new(),
                gaps: Vec::new(),
                checks: Vec::new(),
                last_candidate: None,
                last_verdict: None,
                last_repair: None,
                last_plan: None,
                last_discovery: None,
            },
            events: MemoryEventSink::default(),
            store: EvidenceStore::default(),
            stagnation: StagnationDetector::default(),
            resolver: RoleModelResolver::new(ModelPolicyName::default()),
            panel,
            extra_sink: None,
            drivers,
            clock: 1,
            started_at: Instant::now(),
        };
        supervisor.apply(OrchestrationTransition::BeginContract)?;
        supervisor.emit(
            OrchestrationEventKind::TaskContractCreated,
            "contract accepted",
        )?;
        Ok(supervisor)
    }

    pub fn resume(
        snapshot: OrchestrationSnapshot,
        drivers: SupervisorDrivers,
    ) -> Result<Self, SupervisorError> {
        snapshot
            .contract
            .validate()
            .map_err(SupervisorError::InvalidContract)?;
        Ok(Self {
            panel: VerifierPanel::for_policy(&snapshot.contract.verification_policy),
            snapshot,
            events: MemoryEventSink::default(),
            store: EvidenceStore::default(),
            stagnation: StagnationDetector::default(),
            resolver: RoleModelResolver::new(ModelPolicyName::default()),
            extra_sink: None,
            drivers,
            clock: 1,
            started_at: Instant::now(),
        })
    }

    pub fn attach_sink(&mut self, sink: Box<dyn OrchestrationEventSink>) {
        self.extra_sink = Some(sink);
    }

    pub fn set_resolver(&mut self, resolver: RoleModelResolver) {
        self.resolver = resolver;
    }

    pub fn state(&self) -> OrchestrationState {
        self.snapshot.state
    }

    pub fn snapshot(&self) -> &OrchestrationSnapshot {
        &self.snapshot
    }

    pub fn events(&self) -> &[OrchestrationEvent] {
        &self.events.events
    }

    pub fn panel(&self) -> &VerifierPanel {
        &self.panel
    }

    pub fn resolve_model(&self, role: OrchestrationRole) -> &ModelPolicyName {
        self.resolver.resolve(role)
    }

    pub fn packet(&self, role: OrchestrationRole) -> AgentContextPacket {
        let gaps: Vec<String> = self
            .snapshot
            .gaps
            .iter()
            .filter(|g| g.status == GapStatus::Open)
            .map(|g| g.id.clone())
            .collect();
        AgentContextPacket {
            role,
            task_id: self.snapshot.contract.id,
            objective: self.snapshot.contract.objective.clone(),
            selected_requirements: self
                .snapshot
                .contract
                .requirements
                .iter()
                .map(|r| r.id.clone())
                .collect(),
            evidence_summaries: self
                .snapshot
                .evidence
                .iter()
                .map(|e| e.summary.clone())
                .collect(),
            unresolved_gap_ids: gaps,
            workspace_identity: self.snapshot.current_identity.clone(),
            budget: self.snapshot.contract.resource_budget.clone(),
            instructions: match role {
                OrchestrationRole::Verifier => {
                    "Falsify the claim that every mandatory requirement is satisfied. Do not confirm generously.".into()
                }
                OrchestrationRole::Strategist => {
                    "Diagnose stalled progress. Do not mark the task complete.".into()
                }
                OrchestrationRole::Implementer => {
                    "Produce a candidate completion with evidence. A done claim is not acceptance."
                        .into()
                }
                _ => "Return a typed result. Do not mutate orchestration state.".into(),
            },
        }
    }

    /// Advance through discovery/plan/retrieve/implement according to state.
    pub fn advance(&mut self) -> Result<OrchestrationState, SupervisorError> {
        self.check_budget()?;
        match self.snapshot.state {
            OrchestrationState::Contracting => {
                self.apply(OrchestrationTransition::BeginDiscovery)?;
                let discovery = self
                    .drivers
                    .explorer
                    .explore(&self.packet(OrchestrationRole::Explorer))?;
                self.snapshot.last_discovery = Some(discovery);
                self.emit(OrchestrationEventKind::DiscoveryCompleted, "explorer")?;
            }
            OrchestrationState::Discovering => {
                self.apply(OrchestrationTransition::BeginPlanning)?;
                let plan = self
                    .drivers
                    .planner
                    .plan(&self.packet(OrchestrationRole::Planner))?;
                self.snapshot.last_plan = Some(plan);
                self.emit(OrchestrationEventKind::PlanCreated, "planner")?;
            }
            OrchestrationState::Planning => {
                self.apply(OrchestrationTransition::BeginRetrieval)?;
                let retrieved = self
                    .drivers
                    .retriever
                    .retrieve(&self.packet(OrchestrationRole::Retriever))?;
                self.snapshot.last_discovery = Some(retrieved);
            }
            OrchestrationState::Retrieving => {
                self.apply(OrchestrationTransition::MarkReady)?;
            }
            OrchestrationState::ReadyToImplement => {
                if !profile_allows_writes(OrchestrationRole::Implementer, false) {
                    return Err(SupervisorError::WriteForbidden);
                }
                self.apply(OrchestrationTransition::BeginImplementation)?;
            }
            OrchestrationState::Implementing | OrchestrationState::Repairing => {
                let candidate = self
                    .drivers
                    .implementer
                    .implement(&self.packet(OrchestrationRole::Implementer))?;
                self.submit_candidate(candidate)?;
            }
            _ => {
                return Err(SupervisorError::Transition(
                    TransitionError::InvalidTransition,
                ));
            }
        }
        Ok(self.snapshot.state)
    }

    pub fn submit_candidate(
        &mut self,
        candidate: CandidateCompletion,
    ) -> Result<(), SupervisorError> {
        if candidate.task_id != self.snapshot.contract.id {
            return Err(SupervisorError::InvalidOutput);
        }
        if !matches!(
            self.snapshot.state,
            OrchestrationState::Implementing | OrchestrationState::Repairing
        ) {
            return Err(SupervisorError::Transition(
                TransitionError::InvalidTransition,
            ));
        }
        // A Done claim is recorded, never treated as Accepted.
        let _claim = candidate.completion_claim;
        self.ingest_candidate_evidence(&candidate)?;
        self.snapshot.last_candidate = Some(candidate);
        self.apply(OrchestrationTransition::CollectEvidence)?;
        self.emit(
            OrchestrationEventKind::ImplementationCompleted,
            "candidate recorded",
        )?;
        Ok(())
    }

    pub fn run_checks(&mut self) -> Result<Vec<CheckResult>, SupervisorError> {
        self.apply(OrchestrationTransition::RunChecks)?;
        let requested = self
            .snapshot
            .last_candidate
            .as_ref()
            .ok_or(SupervisorError::MissingCandidate)?
            .checks_requested
            .clone();
        let mut results = Vec::new();
        for id in requested {
            let check = VerificationCheck {
                id: id.clone(),
                kind: crate::orchestration::checks::CheckKind::UnitTest,
                action: id.clone(),
                timeout_ms: 60_000,
                required: true,
                applicable: true,
                expected_result: Some("pass".into()),
            };
            let result = self.drivers.checks.run(&check);
            self.emit(OrchestrationEventKind::CheckCompleted, &result.check_id)?;
            results.push(result);
        }
        self.snapshot.checks = results.clone();
        if self
            .snapshot
            .contract
            .verification_policy
            .deterministic_checks_required
            && results
                .iter()
                .any(|r| !r.status.is_passing() && r.status != CheckStatus::Skipped)
        {
            // Required checks failed: still proceed to verification so the
            // skeptic can emit structured gaps; host will not accept.
        }
        self.apply(OrchestrationTransition::AwaitVerification)?;
        Ok(results)
    }

    pub fn verify(&mut self) -> Result<VerificationVerdict, SupervisorError> {
        match self.snapshot.state {
            OrchestrationState::AwaitingVerification if self.snapshot.round == 0 => {
                self.apply(OrchestrationTransition::BeginVerification)?;
            }
            OrchestrationState::AwaitingVerification => {
                self.apply(OrchestrationTransition::BeginReverification)?;
            }
            _ => {
                return Err(SupervisorError::Transition(
                    TransitionError::InvalidTransition,
                ));
            }
        }
        self.snapshot.round = self.snapshot.round.saturating_add(1);
        let candidate = self
            .snapshot
            .last_candidate
            .clone()
            .ok_or(SupervisorError::MissingCandidate)?;
        let packet = self.packet(OrchestrationRole::Verifier);
        if profile_allows_writes(OrchestrationRole::Verifier, false) {
            return Err(SupervisorError::WriteForbidden);
        }
        let mut verdicts: Vec<VerificationVerdict> = Vec::new();
        for verifier in &self.drivers.verifiers {
            let v = verifier.verify(
                &packet,
                &candidate,
                &self.snapshot.checks,
                &self.snapshot.evidence,
                &self.snapshot.gaps,
            )?;
            verdicts.push(v);
        }
        if verdicts.is_empty() {
            if self.snapshot.contract.verification_policy.skeptic_count == 0 {
                verdicts.push(self.host_verdict(&candidate)?);
            } else {
                return Err(SupervisorError::VerifierUnavailable);
            }
        }
        let mut aggregated = self.aggregate(verdicts, &candidate)?;
        aggregated = self.host_gate(aggregated, &candidate)?;
        self.record_gaps(&aggregated);
        let coverage = self.coverage(&aggregated);
        let verifier_id = aggregated.verifier_id.clone();
        let verifier_model = self
            .resolver
            .resolve(OrchestrationRole::Verifier)
            .as_str()
            .to_owned();
        let ts = self.tick();
        let attestation = Attestation::issue(
            self.snapshot.contract.id,
            &verifier_id,
            &verifier_model,
            self.snapshot.round,
            aggregated.verdict,
            coverage,
            aggregated.evidence_used.clone(),
            aggregated.gaps.iter().map(|g| g.id.clone()).collect(),
            ts,
            self.snapshot.current_identity.clone(),
        );
        // Append-only: never overwrite prior attestations.
        self.snapshot.attestations.push(attestation.clone());
        aggregated.attestation = Some(attestation);
        self.snapshot.last_verdict = Some(aggregated.clone());
        match aggregated.verdict {
            Verdict::Verified => {
                self.apply(OrchestrationTransition::MarkVerified)?;
                self.emit(OrchestrationEventKind::TaskVerified, "verified")?;
            }
            Verdict::Refuted => {
                self.apply(OrchestrationTransition::MarkRefuted)?;
                self.emit(OrchestrationEventKind::VerificationCompleted, "refuted")?;
            }
            Verdict::Inconclusive | Verdict::Blocked => {
                if !self
                    .snapshot
                    .contract
                    .verification_policy
                    .allow_inconclusive_acceptance
                {
                    self.apply(OrchestrationTransition::Block)?;
                    self.emit(OrchestrationEventKind::TaskBlocked, "inconclusive")?;
                }
            }
        }
        if self.snapshot.round >= self.snapshot.contract.verification_policy.max_rounds
            && self.snapshot.state == OrchestrationState::Refuted
        {
            self.apply(OrchestrationTransition::Block)?;
            self.emit(OrchestrationEventKind::TaskBlocked, "max rounds")?;
        }
        Ok(aggregated)
    }

    pub fn repair(&mut self) -> Result<RepairDirective, SupervisorError> {
        if self.snapshot.state != OrchestrationState::Refuted {
            return Err(SupervisorError::Transition(
                TransitionError::InvalidTransition,
            ));
        }
        self.snapshot.repair_rounds = self.snapshot.repair_rounds.saturating_add(1);
        if self.snapshot.repair_rounds > self.snapshot.contract.resource_budget.max_repair_rounds {
            self.apply(OrchestrationTransition::Block)?;
            return Err(SupervisorError::BudgetExceeded);
        }
        let open: Vec<GapNode> = self
            .snapshot
            .gaps
            .iter()
            .filter(|g| g.status == GapStatus::Open)
            .cloned()
            .collect();
        let ids: BTreeSet<String> = open.iter().map(|g| g.id.clone()).collect();
        let _ = self
            .stagnation
            .observe_gaps(StagnationDetector::gap_fingerprint(&ids));
        self.apply(OrchestrationTransition::BeginRepair)?;
        let directive = RepairDirective {
            task_id: self.snapshot.contract.id,
            round: self.snapshot.repair_rounds,
            unresolved_gap_ids: open.iter().map(|g| g.id.clone()).collect(),
            relevant_requirement_ids: open
                .iter()
                .filter_map(|g| g.requirement_id.clone())
                .collect(),
            selected_evidence: self.snapshot.evidence.iter().map(|e| e.id).collect(),
            previous_attempt_summary: self
                .snapshot
                .last_candidate
                .as_ref()
                .map(|c| c.summary.clone())
                .unwrap_or_default(),
            constraints: vec!["do not replay the implementer transcript".into()],
        };
        self.snapshot.last_repair = Some(directive.clone());
        self.emit(OrchestrationEventKind::RepairStarted, "repair")?;
        Ok(directive)
    }

    pub fn strategize(&mut self) -> Result<StrategyRevision, SupervisorError> {
        if !matches!(
            self.snapshot.state,
            OrchestrationState::Refuted | OrchestrationState::Repairing
        ) {
            return Err(SupervisorError::Transition(
                TransitionError::InvalidTransition,
            ));
        }
        if profile_allows_writes(OrchestrationRole::Strategist, false) {
            return Err(SupervisorError::WriteForbidden);
        }
        self.snapshot.strategist_calls = self.snapshot.strategist_calls.saturating_add(1);
        if self.snapshot.strategist_calls
            > self.snapshot.contract.resource_budget.max_strategist_calls
        {
            self.apply(OrchestrationTransition::Block)?;
            return Err(SupervisorError::BudgetExceeded);
        }
        self.apply(OrchestrationTransition::BeginStrategist)?;
        let revision = self.drivers.strategist.revise(
            &self.packet(OrchestrationRole::Strategist),
            &self.snapshot.gaps,
            self.snapshot.repair_rounds,
        )?;
        self.emit(OrchestrationEventKind::StrategistInvoked, "strategist")?;
        match revision.resume_from {
            OrchestrationState::ReadyToImplement => {
                self.apply(OrchestrationTransition::MarkReady)?;
            }
            _ => {
                self.apply(OrchestrationTransition::BeginRepair)?;
            }
        }
        Ok(revision)
    }

    /// Host-only accept. Implementer claims never call this.
    ///
    /// Validation and reduction in one step, for a caller with nothing else
    /// to keep in agreement. A caller that also owns a second projection
    /// (the Runtime Graph) uses [`Self::acceptance_record`] and
    /// [`Self::apply_acceptance`] instead, so one durable acceptance record
    /// can be appended between them and both projections derived from it.
    pub fn accept(&mut self) -> Result<(), SupervisorError> {
        let record = self.acceptance_record()?;
        self.apply_acceptance(&record)
    }

    /// Validate that this run may be accepted and describe the acceptance,
    /// mutating nothing. Every precondition the host gate enforces is
    /// checked here: the state, the recorded verdict, the attestation's
    /// workspace identity, and evidence for every mandatory requirement.
    ///
    /// Returning a record rather than performing the transition is what
    /// makes acceptance event-derived (ADR 0021 §2): the caller appends the
    /// record durably first and only then calls [`Self::apply_acceptance`],
    /// so a crash between the two leaves the durable stream — not a
    /// half-updated projection — as the authority.
    pub fn acceptance_record(&self) -> Result<AcceptanceRecord, SupervisorError> {
        if self.snapshot.state != OrchestrationState::Verified {
            return Err(SupervisorError::Transition(
                TransitionError::InvalidTransition,
            ));
        }
        let policy = &self.snapshot.contract.verification_policy;
        let verdict = self
            .snapshot
            .last_verdict
            .as_ref()
            .ok_or(SupervisorError::MissingEvidence)?;
        if verdict.verdict != Verdict::Verified
            || (policy.acceptance_policy == AcceptancePolicy::Strict
                && verdict.verdict != Verdict::Verified)
        {
            return Err(SupervisorError::MissingEvidence);
        }
        if policy.require_workspace_identity {
            let att = self
                .snapshot
                .attestations
                .last()
                .ok_or(SupervisorError::MissingEvidence)?;
            if att.workspace_identity != self.snapshot.current_identity {
                return Err(SupervisorError::StaleWorkspace);
            }
        }
        if !self.mandatory_evidence_present() {
            return Err(SupervisorError::MissingEvidence);
        }
        let candidate = self
            .snapshot
            .last_candidate
            .as_ref()
            .ok_or(SupervisorError::MissingCandidate)?;
        Ok(AcceptanceRecord {
            task_id: self.snapshot.contract.id,
            verdict: verdict.verdict,
            candidate_digest: candidate_digest(candidate),
            workspace_identity: self.snapshot.current_identity.clone(),
        })
    }

    /// Reduce a durable acceptance record into this snapshot. In-memory
    /// only: the record is already durable when this runs, so nothing here
    /// can fail for want of storage. Refuses a record for another task, and
    /// is idempotent for the record already applied — replaying the durable
    /// stream must not double-apply.
    pub fn apply_acceptance(&mut self, record: &AcceptanceRecord) -> Result<(), SupervisorError> {
        if record.task_id != self.snapshot.contract.id {
            return Err(SupervisorError::InvalidOutput);
        }
        if self.snapshot.state == OrchestrationState::Accepted {
            return Ok(());
        }
        self.apply(OrchestrationTransition::Accept)?;
        self.emit(OrchestrationEventKind::TaskAccepted, "accepted by host")?;
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), SupervisorError> {
        self.apply(OrchestrationTransition::Cancel)?;
        self.emit(OrchestrationEventKind::TaskCancelled, "cancelled")?;
        Ok(())
    }

    pub fn set_current_identity(&mut self, identity: WorkspaceIdentity) {
        self.snapshot.current_identity = identity;
    }

    fn ingest_candidate_evidence(
        &mut self,
        candidate: &CandidateCompletion,
    ) -> Result<(), SupervisorError> {
        if candidate.evidence_refs.len() > crate::orchestration::evidence::MAX_CANDIDATE_EVIDENCE {
            return Err(SupervisorError::InvalidOutput);
        }
        for id in candidate.evidence_refs.iter().chain(
            candidate
                .requirement_claims
                .iter()
                .flat_map(|c| c.evidence_refs.iter()),
        ) {
            if !self.snapshot.evidence.iter().any(|e| e.id == *id) {
                return Err(SupervisorError::MissingEvidence);
            }
        }
        Ok(())
    }

    /// A claim is supported only by a recorded [`EvidenceNode`], never a dangling id.
    fn recorded_evidence_supports(
        &self,
        requirement_id: &str,
        ids: &[protocol::EvidenceId],
    ) -> bool {
        ids.iter().any(|id| {
            self.snapshot.evidence.iter().any(|node| {
                node.id == *id && node.requirement_ids.iter().any(|r| r == requirement_id)
            })
        })
    }

    pub fn record_evidence(&mut self, node: EvidenceNode) -> Result<(), SupervisorError> {
        let kind = node.kind.as_evidence_kind();
        let source = EvidenceSource::new(protocol::ArtifactId::from_bytes(node.summary.as_bytes()));
        let assertion = if kind == crate::evidence::EvidenceKind::Test {
            crate::evidence::TEST_PASSED.to_owned()
        } else {
            node.summary.clone()
        };
        let mut spec = EvidenceSpec::new(
            node.id,
            node.task_id,
            kind,
            assertion,
            EvidenceProducer::System,
            source,
            EvidenceStatus::Passed,
            node.source.clone(),
        )
        .map_err(|_| SupervisorError::InvalidOutput)?;
        if let Some(req) = node.requirement_ids.first() {
            spec = spec
                .with_criterion_id(req.clone())
                .map_err(|_| SupervisorError::InvalidOutput)?;
        }
        spec = spec.with_observed_at(node.timestamp);
        if let Some(command) = &node.command_ref {
            spec = spec
                .with_command(command.clone())
                .map_err(|_| SupervisorError::InvalidOutput)?;
        }
        if let Some(artifact) = node.artifact_ref.clone() {
            spec = spec.with_artifact(artifact);
        }
        self.store
            .record(spec)
            .map_err(|_| SupervisorError::InvalidOutput)?;
        for req in &node.requirement_ids {
            self.snapshot.edges.push(VerificationEdge {
                id: format!("edge-{}-{req}", node.id),
                from_node: req.clone(),
                to_node: node.id.to_string(),
                relation: VerificationRelation::Supports,
                producer: node.producer.clone(),
                timestamp: node.timestamp,
                confidence: Some(100),
            });
        }
        self.snapshot.evidence.push(node);
        self.emit(OrchestrationEventKind::EvidenceCreated, "evidence")?;
        Ok(())
    }

    fn host_verdict(
        &self,
        candidate: &CandidateCompletion,
    ) -> Result<VerificationVerdict, SupervisorError> {
        Ok(self.build_host_gate_verdict(candidate, Verdict::Verified, Vec::new()))
    }

    fn host_gate(
        &self,
        mut verdict: VerificationVerdict,
        candidate: &CandidateCompletion,
    ) -> Result<VerificationVerdict, SupervisorError> {
        let mut gaps = verdict.gaps.clone();
        let mut results = verdict.requirement_results.clone();
        for req in self.snapshot.contract.mandatory_requirement_ids() {
            let claim = candidate
                .requirement_claims
                .iter()
                .find(|c| c.requirement_id == req);
            let has_evidence = claim
                .map(|c| {
                    c.claimed_status == RequirementClaimStatus::Satisfied
                        && self.recorded_evidence_supports(req, &c.evidence_refs)
                })
                .unwrap_or(false);
            if !has_evidence {
                let gap = GapNode::open(
                    self.snapshot.contract.id,
                    Some(req.to_owned()),
                    None,
                    GapCategory::InsufficientEvidence,
                    GapSeverity::High,
                    format!("mandatory {req} has no supporting evidence"),
                    self.snapshot.round,
                );
                gaps.push(gap.clone());
                if let Some(existing) = results.iter_mut().find(|r| r.requirement_id == req) {
                    existing.status = RequirementVerificationStatus::InsufficientEvidence;
                    existing.gap_refs.push(gap.id);
                } else {
                    results.push(RequirementVerification {
                        requirement_id: req.to_owned(),
                        status: RequirementVerificationStatus::InsufficientEvidence,
                        evidence_refs: Vec::new(),
                        gap_refs: vec![gap.id],
                        explanation: "host rejected claim without evidence".into(),
                    });
                }
            }
        }
        if self
            .snapshot
            .contract
            .verification_policy
            .deterministic_checks_required
            && self
                .snapshot
                .checks
                .iter()
                .any(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Timeout))
        {
            verdict.verdict = Verdict::Refuted;
        }
        if results
            .iter()
            .any(|r| r.status != RequirementVerificationStatus::Satisfied)
            || !gaps.is_empty()
        {
            verdict.verdict = Verdict::Refuted;
        }
        verdict.gaps = gaps;
        verdict.requirement_results = results;
        Ok(verdict)
    }

    fn build_host_gate_verdict(
        &self,
        candidate: &CandidateCompletion,
        verdict: Verdict,
        gaps: Vec<GapNode>,
    ) -> VerificationVerdict {
        let results = self
            .snapshot
            .contract
            .requirements
            .iter()
            .map(|r| RequirementVerification {
                requirement_id: r.id.clone(),
                status: RequirementVerificationStatus::Satisfied,
                evidence_refs: candidate.evidence_refs.clone(),
                gap_refs: Vec::new(),
                explanation: "host trivial-complexity gate".into(),
            })
            .collect();
        VerificationVerdict {
            task_id: self.snapshot.contract.id,
            verifier_id: "host".into(),
            verdict,
            requirement_results: results,
            evidence_used: candidate.evidence_refs.clone(),
            gaps,
            confidence: 100,
            notes: String::new(),
            attestation: None,
        }
    }

    fn aggregate(
        &self,
        verdicts: Vec<VerificationVerdict>,
        _candidate: &CandidateCompletion,
    ) -> Result<VerificationVerdict, SupervisorError> {
        match self.panel.aggregation_policy {
            AggregationPolicy::AnyRefutationFails | AggregationPolicy::Unanimous => {
                if verdicts.iter().any(|v| v.verdict == Verdict::Refuted) {
                    let mut chosen = verdicts
                        .into_iter()
                        .find(|v| v.verdict == Verdict::Refuted)
                        .expect("refuted");
                    chosen.verdict = Verdict::Refuted;
                    return Ok(chosen);
                }
                if verdicts.iter().any(|v| v.verdict != Verdict::Verified)
                    && self.panel.aggregation_policy == AggregationPolicy::Unanimous
                {
                    let mut chosen = verdicts.into_iter().next().expect("verdict");
                    chosen.verdict = Verdict::Inconclusive;
                    return Ok(chosen);
                }
                Ok(verdicts.into_iter().next().expect("verdict"))
            }
            _ => Ok(verdicts.into_iter().next().expect("verdict")),
        }
    }

    fn record_gaps(&mut self, verdict: &VerificationVerdict) {
        for incoming in &verdict.gaps {
            if let Some(existing) = self.snapshot.gaps.iter_mut().find(|g| g.id == incoming.id) {
                existing.last_seen_round = self.snapshot.round;
                existing.status = GapStatus::Open;
            } else {
                let mut gap = incoming.clone();
                gap.first_seen_round = self.snapshot.round;
                gap.last_seen_round = self.snapshot.round;
                self.snapshot.gaps.push(gap);
                let _ = self.emit(OrchestrationEventKind::GapCreated, "gap");
            }
        }
        let incoming_ids: BTreeSet<&str> = verdict.gaps.iter().map(|g| g.id.as_str()).collect();
        for gap in &mut self.snapshot.gaps {
            if gap.status == GapStatus::Open && !incoming_ids.contains(gap.id.as_str()) {
                gap.status = GapStatus::Resolved;
            }
        }
    }

    fn coverage(&self, verdict: &VerificationVerdict) -> u8 {
        let total = self.snapshot.contract.requirements.len().max(1);
        let ok = verdict
            .requirement_results
            .iter()
            .filter(|r| r.status == RequirementVerificationStatus::Satisfied)
            .count();
        ((ok * 100) / total) as u8
    }

    fn mandatory_evidence_present(&self) -> bool {
        let Some(candidate) = &self.snapshot.last_candidate else {
            return false;
        };
        self.snapshot
            .contract
            .mandatory_requirement_ids()
            .into_iter()
            .all(|req| {
                candidate.requirement_claims.iter().any(|c| {
                    c.requirement_id == req
                        && c.claimed_status == RequirementClaimStatus::Satisfied
                        && self.recorded_evidence_supports(req, &c.evidence_refs)
                })
            })
    }

    fn check_budget(&self) -> Result<(), SupervisorError> {
        let b = &self.snapshot.contract.resource_budget;
        if self.snapshot.round > b.max_verification_rounds
            || self.snapshot.repair_rounds > b.max_repair_rounds
            || self.snapshot.strategist_calls > b.max_strategist_calls
            || self.snapshot.tokens_used > b.max_tokens
            || self.started_at.elapsed() > Duration::from_millis(b.max_wall_clock_ms)
        {
            return Err(SupervisorError::BudgetExceeded);
        }
        Ok(())
    }

    fn apply(&mut self, transition: OrchestrationTransition) -> Result<(), SupervisorError> {
        let next = validate_transition(self.snapshot.state, transition)
            .map_err(SupervisorError::Transition)?;
        self.snapshot.state = next;
        Ok(())
    }

    fn emit(&mut self, kind: OrchestrationEventKind, summary: &str) -> Result<(), SupervisorError> {
        let event = OrchestrationEvent {
            kind,
            task_id: self.snapshot.contract.id,
            round: self.snapshot.round,
            summary: summary.to_owned(),
        };
        self.events.emit(&event).map_err(SupervisorError::Sink)?;
        if let Some(sink) = &mut self.extra_sink {
            sink.emit(&event).map_err(SupervisorError::Sink)?;
        }
        Ok(())
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("verified orchestration is disabled"),
            Self::InvalidContract(err) => write!(f, "{err}"),
            Self::Transition(err) => write!(f, "{err}"),
            Self::MissingCandidate => f.write_str("missing candidate completion"),
            Self::MissingEvidence => f.write_str("mandatory evidence missing"),
            Self::StaleWorkspace => f.write_str("attestation workspace identity is stale"),
            Self::BudgetExceeded => f.write_str("orchestration budget exceeded"),
            Self::InvalidOutput => f.write_str("invalid structured agent output"),
            Self::VerifierUnavailable => f.write_str("verifier unavailable"),
            Self::WriteForbidden => f.write_str("role is not permitted to write"),
            Self::Sink(err) => write!(f, "event sink: {err}"),
            Self::Agent(err) => write!(f, "agent: {err}"),
        }
    }
}

impl Error for SupervisorError {}

// EvidenceSpec / EvidenceSource fields are crate-private. Keep a local helper
// that records into EvidenceStore when constructors exist; otherwise the
// orchestration EvidenceNode list is the source of truth and maps onto
// EvidenceKind via OrchestrationEvidenceKind::as_evidence_kind.

impl SupervisorDrivers {
    #[cfg(test)]
    pub fn fakes(script: FakeScript) -> Self {
        Self {
            planner: Box::new(FakePlanner),
            explorer: Box::new(FakeExplorer),
            retriever: Box::new(FakeRetriever),
            implementer: Box::new(FakeImplementer {
                task_id: script.task_id,
                evidence: script.evidence.clone(),
                include_evidence: script.include_evidence,
            }),
            verifiers: vec![Box::new(FakeVerifier {
                refute_n: script.refute_n,
                seen: std::cell::Cell::new(0),
                task_id: script.task_id,
            })],
            strategist: Box::new(FakeStrategist),
            checks: Box::new(FakeCheckRunner {
                fail: script.fail_checks,
            }),
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeScript {
    pub task_id: GoalId,
    pub evidence: Vec<EvidenceId>,
    pub include_evidence: bool,
    pub refute_n: u32,
    pub fail_checks: bool,
}

#[cfg(test)]
struct FakePlanner;
#[cfg(test)]
impl Planner for FakePlanner {
    fn plan(&self, _packet: &AgentContextPacket) -> Result<PlanResult, SupervisorError> {
        Ok(PlanResult {
            summary: "plan".into(),
            steps: vec!["implement".into()],
        })
    }
}

#[cfg(test)]
struct FakeExplorer;
#[cfg(test)]
impl Explorer for FakeExplorer {
    fn explore(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "explored".into(),
            files: vec!["src/lib.rs".into()],
            symbols: vec!["Supervisor".into()],
        })
    }
}

#[cfg(test)]
struct FakeRetriever;
#[cfg(test)]
impl Retriever for FakeRetriever {
    fn retrieve(&self, _packet: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
        Ok(DiscoveryResult {
            summary: "retrieved".into(),
            files: vec!["src/lib.rs".into()],
            symbols: vec![],
        })
    }
}

#[cfg(test)]
struct FakeImplementer {
    task_id: GoalId,
    evidence: Vec<EvidenceId>,
    include_evidence: bool,
}
#[cfg(test)]
impl Implementer for FakeImplementer {
    fn implement(
        &self,
        packet: &AgentContextPacket,
    ) -> Result<CandidateCompletion, SupervisorError> {
        let refs = if self.include_evidence {
            self.evidence.clone()
        } else {
            Vec::new()
        };
        let claims = packet
            .selected_requirements
            .iter()
            .map(|id| crate::orchestration::evidence::RequirementClaim {
                requirement_id: id.clone(),
                claimed_status: RequirementClaimStatus::Satisfied,
                evidence_refs: refs.clone(),
                explanation: "fake implementer".into(),
            })
            .collect();
        Ok(CandidateCompletion {
            task_id: self.task_id,
            summary: "implemented".into(),
            changed_artifacts: vec!["src/lib.rs".into()],
            requirement_claims: claims,
            evidence_refs: refs,
            checks_requested: vec!["unit".into()],
            known_limitations: Vec::new(),
            unresolved_items: Vec::new(),
            completion_claim: CompletionClaim::Done,
        })
    }
}

#[cfg(test)]
struct FakeVerifier {
    refute_n: u32,
    seen: std::cell::Cell<u32>,
    task_id: GoalId,
}
#[cfg(test)]
impl Verifier for FakeVerifier {
    fn verify(
        &self,
        packet: &AgentContextPacket,
        candidate: &CandidateCompletion,
        _checks: &[CheckResult],
        _evidence: &[EvidenceNode],
        _previous_gaps: &[GapNode],
    ) -> Result<VerificationVerdict, SupervisorError> {
        let n = self.seen.get();
        self.seen.set(n + 1);
        if n < self.refute_n {
            let gaps: Vec<GapNode> = packet
                .selected_requirements
                .iter()
                .map(|req| {
                    GapNode::open(
                        self.task_id,
                        Some(req.clone()),
                        None,
                        GapCategory::IncorrectImplementation,
                        GapSeverity::High,
                        "refuted by fake skeptic".into(),
                        n + 1,
                    )
                })
                .collect();
            let results = packet
                .selected_requirements
                .iter()
                .map(|req| RequirementVerification {
                    requirement_id: req.clone(),
                    status: RequirementVerificationStatus::Unsatisfied,
                    evidence_refs: Vec::new(),
                    gap_refs: gaps.iter().map(|g| g.id.clone()).collect(),
                    explanation: "skeptic refutation".into(),
                })
                .collect();
            return Ok(VerificationVerdict {
                task_id: self.task_id,
                verifier_id: "fake-verifier".into(),
                verdict: Verdict::Refuted,
                requirement_results: results,
                evidence_used: candidate.evidence_refs.clone(),
                gaps,
                confidence: 90,
                notes: String::new(),
                attestation: None,
            });
        }
        let results = packet
            .selected_requirements
            .iter()
            .map(|req| RequirementVerification {
                requirement_id: req.clone(),
                status: RequirementVerificationStatus::Satisfied,
                evidence_refs: candidate.evidence_refs.clone(),
                gap_refs: Vec::new(),
                explanation: "skeptic could not falsify".into(),
            })
            .collect();
        Ok(VerificationVerdict {
            task_id: self.task_id,
            verifier_id: "fake-verifier".into(),
            verdict: Verdict::Verified,
            requirement_results: results,
            evidence_used: candidate.evidence_refs.clone(),
            gaps: Vec::new(),
            confidence: 80,
            notes: String::new(),
            attestation: None,
        })
    }
}

#[cfg(test)]
struct FakeStrategist;
#[cfg(test)]
impl Strategist for FakeStrategist {
    fn revise(
        &self,
        _packet: &AgentContextPacket,
        _gaps: &[GapNode],
        _attempts: u32,
    ) -> Result<StrategyRevision, SupervisorError> {
        Ok(StrategyRevision {
            summary: "change approach".into(),
            constraints: vec!["narrow the repair".into()],
            resume_from: OrchestrationState::Repairing,
        })
    }
}

#[cfg(test)]
struct FakeCheckRunner {
    fail: bool,
}
#[cfg(test)]
impl CheckRunner for FakeCheckRunner {
    fn run(&self, check: &VerificationCheck) -> CheckResult {
        CheckResult {
            check_id: check.id.clone(),
            status: if self.fail {
                CheckStatus::Failed
            } else {
                CheckStatus::Passed
            },
            exit_code: Some(if self.fail { 1 } else { 0 }),
            duration_ms: 5,
            stdout_ref: Some("artifact:stdout".into()),
            stderr_ref: None,
            evidence_id: EvidenceId::new(),
            failure_summary: if self.fail {
                Some("test failed".into())
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::contract::{AcceptanceCriterion, RequirementNode};
    use crate::orchestration::evidence::{
        CompletionClaim, EvidenceTrust, OrchestrationEvidenceKind,
    };
    use crate::orchestration::policy::{TaskComplexity, VerificationPolicy};
    use crate::orchestration::state::TransitionError;
    use protocol::{EvidenceId, GoalId, RapidConfig};

    fn contract(id: GoalId) -> TaskContract {
        TaskContract {
            id,
            parent_task_id: None,
            objective: "ship verified orchestration".into(),
            scope: vec!["crates/agent-runtime".into()],
            constraints: vec![],
            requirements: vec![RequirementNode {
                id: "REQ-001".into(),
                description: "host owns acceptance".into(),
                source: "prd".into(),
                priority: crate::orchestration::contract::RequirementPriority::High,
                mandatory: true,
                depends_on: vec![],
                acceptance_criteria: vec!["AC-001".into()],
            }],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-001".into(),
                text: "accept only after verified".into(),
            }],
            permitted_capabilities: vec!["fs.read".into()],
            required_capabilities: vec![],
            forbidden_actions: vec![],
            expected_outputs: vec!["docs".into()],
            verification_policy: VerificationPolicy::for_complexity(TaskComplexity::Substantial),
            resource_budget: OrchestrationBudget::default(),
            context_budget: 8_000,
            workspace_policy: crate::orchestration::contract::WorkspacePolicy::Isolated,
            complexity: TaskComplexity::Substantial,
            metadata: BTreeSet::new(),
        }
    }

    fn identity() -> WorkspaceIdentity {
        WorkspaceIdentity::new(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
    }

    fn evidence_id() -> EvidenceId {
        EvidenceId::new()
    }

    fn start_with(script: FakeScript) -> Supervisor {
        let c = contract(script.task_id);
        Supervisor::start(c, identity(), SupervisorDrivers::fakes(script)).expect("start")
    }

    fn drive_to_implementing(sup: &mut Supervisor) {
        assert_eq!(sup.state(), OrchestrationState::Contracting);
        assert_eq!(sup.advance().unwrap(), OrchestrationState::Discovering);
        assert_eq!(sup.advance().unwrap(), OrchestrationState::Planning);
        assert_eq!(sup.advance().unwrap(), OrchestrationState::Retrieving);
        assert_eq!(sup.advance().unwrap(), OrchestrationState::ReadyToImplement);
        assert_eq!(sup.advance().unwrap(), OrchestrationState::Implementing);
    }

    fn record_supporting_evidence(sup: &mut Supervisor, id: EvidenceId) {
        sup.record_evidence(EvidenceNode {
            id,
            task_id: sup.snapshot().contract.id,
            requirement_ids: vec!["REQ-001".into()],
            producer: "implementer".into(),
            kind: OrchestrationEvidenceKind::Test,
            source: "cargo test".into(),
            timestamp: 1,
            artifact_ref: None,
            command_ref: Some("cargo test".into()),
            workspace_ref: Some(identity()),
            content_hash: Some(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ),
            summary: "tests passed".into(),
            structured_payload: None,
            trust_level: EvidenceTrust::Deterministic,
        })
        .unwrap();
    }

    #[test]
    fn default_config_does_not_construct_supervisor() {
        let id = GoalId::new();
        let eid = evidence_id();
        let drivers = SupervisorDrivers::fakes(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 0,
            fail_checks: false,
        });
        let built =
            Supervisor::from_config(&RapidConfig::default(), identity(), contract(id), drivers)
                .expect("from_config");
        assert!(built.is_none());
        assert_eq!(
            RapidConfig::default().orchestration.mode,
            OrchestrationMode::Off
        );
    }

    #[test]
    fn start_rejects_fewer_verifiers_than_skeptic_count_requires() {
        // `Critical` complexity requires 2 independent skeptics
        // (`Unanimous` aggregation, `skeptic_count: 2`), but
        // `SupervisorDrivers::fakes` always supplies exactly one verifier.
        // `start()` must reject this under-provisioned combination rather
        // than silently accepting it and letting a single verdict satisfy
        // what the policy calls for two-must-agree.
        let id = GoalId::new();
        let mut c = contract(id);
        c.verification_policy = VerificationPolicy::for_complexity(TaskComplexity::Critical);
        let drivers = SupervisorDrivers::fakes(FakeScript {
            task_id: id,
            evidence: vec![],
            include_evidence: false,
            refute_n: 0,
            fail_checks: false,
        });
        match Supervisor::start(c, identity(), drivers) {
            Err(SupervisorError::VerifierUnavailable) => {}
            other => panic!(
                "expected Err(VerifierUnavailable) for an under-provisioned panel, got is_ok={}",
                other.is_ok()
            ),
        }
    }

    #[test]
    fn happy_path_accepts_only_via_host() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 0,
            fail_checks: false,
        });
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        // Implementer Done claim must not accept.
        assert_eq!(
            sup.advance().unwrap(),
            OrchestrationState::CollectingEvidence
        );
        assert_eq!(
            sup.snapshot()
                .last_candidate
                .as_ref()
                .unwrap()
                .completion_claim,
            CompletionClaim::Done
        );
        assert_ne!(sup.state(), OrchestrationState::Accepted);
        assert!(sup.accept().is_err());
        sup.run_checks().unwrap();
        let verdict = sup.verify().unwrap();
        assert_eq!(verdict.verdict, Verdict::Verified);
        assert_eq!(sup.state(), OrchestrationState::Verified);
        sup.accept().unwrap();
        assert_eq!(sup.state(), OrchestrationState::Accepted);
        assert!(
            sup.events()
                .iter()
                .any(|e| e.kind == OrchestrationEventKind::TaskAccepted)
        );
        assert!(
            !sup.events()
                .iter()
                .any(|e| e.kind == OrchestrationEventKind::TaskAccepted)
                || sup
                    .snapshot()
                    .last_candidate
                    .as_ref()
                    .unwrap()
                    .completion_claim
                    == CompletionClaim::Done
        );
        assert!(!sup.snapshot().attestations.is_empty());
        assert_eq!(
            sup.snapshot().attestations[0].workspace_identity,
            identity()
        );
        assert!(!profile_allows_writes(OrchestrationRole::Verifier, false));
        assert!(profile_allows_writes(OrchestrationRole::Implementer, false));
        assert_eq!(
            sup.resolve_model(OrchestrationRole::Verifier).as_str(),
            "balanced"
        );
        assert!(
            !sup.packet(OrchestrationRole::Verifier)
                .instructions
                .contains("implementer transcript")
        );
    }

    #[test]
    fn refute_repair_reverify_accept() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 1,
            fail_checks: false,
        });
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        let first = sup.verify().unwrap();
        assert_eq!(first.verdict, Verdict::Refuted);
        assert_eq!(sup.state(), OrchestrationState::Refuted);
        assert!(!first.gaps.is_empty());
        let gap_id = first.gaps[0].id.clone();
        let directive = sup.repair().unwrap();
        assert!(directive.unresolved_gap_ids.contains(&gap_id));
        assert_eq!(sup.state(), OrchestrationState::Repairing);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        let second = sup.verify().unwrap();
        assert_eq!(second.verdict, Verdict::Verified);
        sup.accept().unwrap();
        assert_eq!(sup.state(), OrchestrationState::Accepted);
        assert_eq!(
            sup.snapshot()
                .gaps
                .iter()
                .find(|g| g.id == gap_id)
                .unwrap()
                .id,
            gap_id
        );
    }

    #[test]
    fn repeated_refute_invokes_strategist() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 8,
            fail_checks: false,
        });
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        sup.verify().unwrap();
        let _ = sup.repair().unwrap();
        let revision = sup.strategize().unwrap();
        assert_eq!(revision.resume_from, OrchestrationState::Repairing);
        assert_eq!(sup.state(), OrchestrationState::Repairing);
        assert!(
            sup.events()
                .iter()
                .any(|e| e.kind == OrchestrationEventKind::StrategistInvoked)
        );
    }

    #[test]
    fn max_verification_rounds_block() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut c = contract(id);
        c.verification_policy.max_rounds = 1;
        let mut sup = Supervisor::start(
            c,
            identity(),
            SupervisorDrivers::fakes(FakeScript {
                task_id: id,
                evidence: vec![eid],
                include_evidence: true,
                refute_n: 8,
                fail_checks: false,
            }),
        )
        .unwrap();
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        sup.verify().unwrap();
        assert_eq!(sup.state(), OrchestrationState::Blocked);
    }

    #[test]
    fn stale_workspace_cannot_accept() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 0,
            fail_checks: false,
        });
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        sup.verify().unwrap();
        sup.set_current_identity(WorkspaceIdentity::new(
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        ));
        assert_eq!(sup.accept(), Err(SupervisorError::StaleWorkspace));
        assert_eq!(sup.state(), OrchestrationState::Verified);
    }

    #[test]
    fn missing_mandatory_evidence_cannot_accept() {
        let id = GoalId::new();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![],
            include_evidence: false,
            refute_n: 0,
            fail_checks: false,
        });
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        let verdict = sup.verify().unwrap();
        assert_eq!(verdict.verdict, Verdict::Refuted);
        assert!(
            verdict
                .gaps
                .iter()
                .any(|g| g.category == GapCategory::InsufficientEvidence)
        );
        assert!(sup.accept().is_err());
    }

    #[test]
    fn dangling_evidence_id_cannot_verify_or_accept() {
        let id = GoalId::new();
        let dangling = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![dangling],
            include_evidence: true,
            refute_n: 0,
            fail_checks: false,
        });
        drive_to_implementing(&mut sup);
        let err = sup.advance().expect_err("dangling evidence id");
        assert_eq!(err, SupervisorError::MissingEvidence);
        assert_ne!(sup.state(), OrchestrationState::Accepted);
        assert_ne!(sup.state(), OrchestrationState::Verified);
        assert!(sup.verify().is_err());
        assert_eq!(
            sup.accept(),
            Err(SupervisorError::Transition(
                TransitionError::InvalidTransition
            ))
        );
    }

    #[test]
    fn invalid_transition_fails_closed() {
        let id = GoalId::new();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![],
            include_evidence: false,
            refute_n: 0,
            fail_checks: false,
        });
        assert!(sup.verify().is_err());
        assert!(sup.accept().is_err());
        sup.cancel().unwrap();
        assert_eq!(sup.state(), OrchestrationState::Cancelled);
        assert!(sup.advance().is_err());
    }

    #[test]
    fn resume_restores_non_terminal_state() {
        let id = GoalId::new();
        let eid = evidence_id();
        let mut sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![eid],
            include_evidence: true,
            refute_n: 1,
            fail_checks: false,
        });
        record_supporting_evidence(&mut sup, eid);
        drive_to_implementing(&mut sup);
        sup.advance().unwrap();
        sup.run_checks().unwrap();
        sup.verify().unwrap();
        assert_eq!(sup.state(), OrchestrationState::Refuted);
        let snap = sup.snapshot().clone();
        let resumed = Supervisor::resume(
            snap,
            SupervisorDrivers::fakes(FakeScript {
                task_id: id,
                evidence: vec![eid],
                include_evidence: true,
                refute_n: 0,
                fail_checks: false,
            }),
        )
        .unwrap();
        assert_eq!(resumed.state(), OrchestrationState::Refuted);
    }

    #[test]
    fn verifier_context_excludes_implementer_conversation() {
        let id = GoalId::new();
        let sup = start_with(FakeScript {
            task_id: id,
            evidence: vec![],
            include_evidence: false,
            refute_n: 0,
            fail_checks: false,
        });
        let packet = sup.packet(OrchestrationRole::Verifier);
        assert_eq!(packet.role, OrchestrationRole::Verifier);
        assert!(packet.instructions.contains("Falsify"));
        assert!(!packet.instructions.to_lowercase().contains("transcript"));
        assert!(packet.unresolved_gap_ids.is_empty());
    }

    #[test]
    fn panel_supports_multiple_assignments() {
        let policy = VerificationPolicy::for_complexity(TaskComplexity::Critical);
        let panel = VerifierPanel::for_policy(&policy);
        assert!(panel.assignments.len() >= 2);
        assert_eq!(panel.aggregation_policy, AggregationPolicy::Unanimous);
    }

    #[test]
    fn wall_clock_budget_forces_blocked_not_an_unbounded_run() {
        let id = GoalId::new();
        let mut c = contract(id);
        c.resource_budget = OrchestrationBudget {
            max_wall_clock_ms: 1,
            ..OrchestrationBudget::default()
        };
        let drivers = SupervisorDrivers::fakes(FakeScript {
            task_id: id,
            evidence: vec![],
            include_evidence: false,
            refute_n: 0,
            fail_checks: false,
        });
        let mut sup = Supervisor::start(c, identity(), drivers).expect("start");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(sup.advance(), Err(SupervisorError::BudgetExceeded));
    }
}
