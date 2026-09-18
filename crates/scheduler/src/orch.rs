//! Wire the existing verified-orchestration Supervisor through the Runtime Graph.
//!
//! KEEP the supervisor. This module is an adapter, not a second engine.

use agent_runtime::{
    OrchestrationState, Supervisor, SupervisorDrivers, SupervisorError, TaskContract,
    WorkspaceIdentity,
};
use event_ledger::ledger::EventLedger;
use protocol::{GraphId, NodeId, ProjectId, SessionId};

use crate::kinds::{EdgeKind, NodeKind, NodeState};
use crate::proposal::{EdgeSpec, GraphProposal, NodeSpec, ProposalError};
use crate::service::{GraphError, GraphService};

/// The task/verification nodes a run starts with, hanging off the goal root
/// the graph is created with. Edges are among plan nodes; every node without
/// an incoming plan edge is wired `root --DecomposesInto--> node` by
/// [`GraphBackedRun::start_planned`] so the whole plan is reachable from the
/// goal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunPlan {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
}

impl RunPlan {
    /// The default shape: one `implement` task and one `verify` node that
    /// depends on it.
    pub fn implement_and_verify() -> Self {
        let task = NodeId::new();
        let verify = NodeId::new();
        Self {
            nodes: vec![
                NodeSpec::new(task, NodeKind::Task, "implement"),
                NodeSpec::new(verify, NodeKind::Verification, "verify"),
            ],
            edges: vec![EdgeSpec {
                from: task,
                to: verify,
                kind: EdgeKind::DependsOn,
                condition: crate::graph::EdgeCondition::default(),
            }],
        }
    }
}

/// Graph-backed run: Goal/Task/Verification nodes plus the existing Supervisor.
pub struct GraphBackedRun {
    pub graphs: GraphService,
    pub graph_id: GraphId,
    pub goal_node: NodeId,
    /// Plan nodes that are not verification nodes, in plan order (the
    /// default shape has one, `implement`).
    pub task_nodes: Vec<NodeId>,
    /// Verification nodes in plan order (the default shape has one,
    /// `verify`). [`Self::accept`] refuses while any of them is in a
    /// non-successful terminal state.
    pub verify_nodes: Vec<NodeId>,
    pub supervisor: Supervisor,
}

impl GraphBackedRun {
    pub fn start(
        contract: TaskContract,
        identity: WorkspaceIdentity,
        drivers: SupervisorDrivers,
        ledger: EventLedger,
        session: SessionId,
        project: ProjectId,
    ) -> Result<Self, GraphError> {
        Self::start_planned(
            contract,
            identity,
            drivers,
            ledger,
            session,
            project,
            RunPlan::implement_and_verify(),
        )
    }

    /// Start a run over the caller's plan (a playbook's steps, say) instead
    /// of the default implement/verify pair. The graph is created and the
    /// plan proposed — both durable before the supervisor exists — and the
    /// proposal is validated by the graph service like any other (unknown
    /// nodes, cycles, duplicate ids fail closed).
    pub fn start_planned(
        contract: TaskContract,
        identity: WorkspaceIdentity,
        drivers: SupervisorDrivers,
        ledger: EventLedger,
        session: SessionId,
        project: ProjectId,
        plan: RunPlan,
    ) -> Result<Self, GraphError> {
        if plan.nodes.is_empty() {
            return Err(GraphError::Proposal(ProposalError::UnknownNode));
        }
        let mut graphs = GraphService::open(ledger, session, project)?;
        let created = graphs.create(&contract.objective)?;
        let graph_id = created.graph_id;
        let goal_node = created.root;
        let mut add_edges: Vec<EdgeSpec> = plan
            .nodes
            .iter()
            .filter(|node| !plan.edges.iter().any(|edge| edge.to == node.id))
            .map(|node| EdgeSpec {
                from: goal_node,
                to: node.id,
                kind: EdgeKind::DecomposesInto,
                condition: crate::graph::EdgeCondition::default(),
            })
            .collect();
        add_edges.extend(plan.edges.iter().cloned());
        let task_nodes = plan
            .nodes
            .iter()
            .filter(|node| node.kind != NodeKind::Verification)
            .map(|node| node.id)
            .collect();
        let verify_nodes = plan
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Verification)
            .map(|node| node.id)
            .collect();
        graphs.propose(
            graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: plan.nodes,
                add_edges,
                supersede: vec![],
                invalidate: vec![],
            },
        )?;
        let supervisor = Supervisor::start(contract, identity, drivers)
            .map_err(|e| GraphError::Ledger(e.to_string()))?;
        Ok(Self {
            graphs,
            graph_id,
            goal_node,
            task_nodes,
            verify_nodes,
            supervisor,
        })
    }

    /// Host-only acceptance: the supervisor accepts (it alone can reach
    /// `Accepted`), then the goal and any verification node that has not
    /// already succeeded are marked succeeded on the graph. A verification
    /// node that failed, was cancelled, superseded or invalidated blocks
    /// acceptance before anything is touched — the graph's own record of a
    /// failed check is never overwritten by an accept.
    pub fn accept(&mut self) -> Result<(), SupervisorError> {
        let snapshot = self
            .graphs
            .snapshot(self.graph_id)
            .map_err(|e| SupervisorError::Sink(e.to_string()))?;
        let blocked = self.verify_nodes.iter().any(|id| {
            snapshot.node(*id).is_some_and(|node| {
                matches!(
                    node.state,
                    NodeState::Failed
                        | NodeState::Cancelled
                        | NodeState::Superseded
                        | NodeState::Invalidated
                )
            })
        });
        if blocked {
            return Err(SupervisorError::MissingEvidence);
        }
        let pending: Vec<NodeId> = self
            .verify_nodes
            .iter()
            .copied()
            .filter(|id| {
                snapshot
                    .node(*id)
                    .is_some_and(|node| node.state != NodeState::Succeeded)
            })
            .collect();
        self.supervisor.accept()?;
        self.graphs
            .set_state(self.graph_id, self.goal_node, NodeState::Succeeded)
            .map_err(|e| SupervisorError::Sink(e.to_string()))?;
        for id in pending {
            self.graphs
                .set_state(self.graph_id, id, NodeState::Succeeded)
                .map_err(|e| SupervisorError::Sink(e.to_string()))?;
        }
        Ok(())
    }

    pub fn orchestration_state(&self) -> OrchestrationState {
        self.supervisor.state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::NodeKind;
    use agent_runtime::orchestration::{
        AcceptanceCriterion, AgentContextPacket, CandidateCompletion, CheckResult, CheckRunner,
        CompletionClaim, DiscoveryResult, EvidenceNode, Explorer, Implementer, OrchestrationBudget,
        PlanResult, Planner, RequirementClaim, RequirementClaimStatus, RequirementNode,
        RequirementPriority, RequirementVerification, RequirementVerificationStatus, Retriever,
        Strategist, StrategyRevision, SupervisorError, TaskComplexity, TaskContract, Verdict,
        VerificationCheck, VerificationPolicy, VerificationVerdict, Verifier, WorkspaceIdentity,
        WorkspacePolicy,
    };
    use event_ledger::event::EventKind;
    use event_ledger::ledger::EventLedger;
    use protocol::{EvidenceId, GoalId, ProjectId, SessionId};
    use std::collections::BTreeSet;
    use std::fs;

    struct NopPlanner;
    impl Planner for NopPlanner {
        fn plan(&self, _: &AgentContextPacket) -> Result<PlanResult, SupervisorError> {
            Ok(PlanResult {
                summary: "p".into(),
                steps: vec![],
            })
        }
    }
    struct NopExplore;
    impl Explorer for NopExplore {
        fn explore(&self, _: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
            Ok(DiscoveryResult {
                summary: "e".into(),
                files: vec![],
                symbols: vec![],
            })
        }
    }
    impl Retriever for NopExplore {
        fn retrieve(&self, _: &AgentContextPacket) -> Result<DiscoveryResult, SupervisorError> {
            Ok(DiscoveryResult {
                summary: "r".into(),
                files: vec![],
                symbols: vec![],
            })
        }
    }
    struct NopImpl {
        id: GoalId,
        ev: EvidenceId,
    }
    impl Implementer for NopImpl {
        fn implement(
            &self,
            p: &AgentContextPacket,
        ) -> Result<CandidateCompletion, SupervisorError> {
            Ok(CandidateCompletion {
                task_id: self.id,
                summary: "i".into(),
                changed_artifacts: vec![],
                requirement_claims: p
                    .selected_requirements
                    .iter()
                    .map(|r| RequirementClaim {
                        requirement_id: r.clone(),
                        claimed_status: RequirementClaimStatus::Satisfied,
                        evidence_refs: vec![self.ev],
                        explanation: "n".into(),
                    })
                    .collect(),
                evidence_refs: vec![self.ev],
                checks_requested: vec!["unit".into()],
                known_limitations: vec![],
                unresolved_items: vec![],
                completion_claim: CompletionClaim::Done,
            })
        }
    }
    struct NopVer {
        id: GoalId,
        ev: EvidenceId,
    }
    impl Verifier for NopVer {
        fn verify(
            &self,
            p: &AgentContextPacket,
            c: &CandidateCompletion,
            _: &[CheckResult],
            _: &[EvidenceNode],
            _: &[agent_runtime::orchestration::GapNode],
        ) -> Result<VerificationVerdict, SupervisorError> {
            Ok(VerificationVerdict {
                task_id: self.id,
                verifier_id: "v".into(),
                verdict: Verdict::Verified,
                requirement_results: p
                    .selected_requirements
                    .iter()
                    .map(|r| RequirementVerification {
                        requirement_id: r.clone(),
                        status: RequirementVerificationStatus::Satisfied,
                        evidence_refs: vec![self.ev],
                        gap_refs: vec![],
                        explanation: "ok".into(),
                    })
                    .collect(),
                evidence_used: c.evidence_refs.clone(),
                gaps: vec![],
                confidence: 80,
                notes: String::new(),
                attestation: None,
            })
        }
    }
    struct NopStrat;
    impl Strategist for NopStrat {
        fn revise(
            &self,
            _: &AgentContextPacket,
            _: &[agent_runtime::orchestration::GapNode],
            _: u32,
        ) -> Result<StrategyRevision, SupervisorError> {
            Ok(StrategyRevision {
                summary: "s".into(),
                constraints: vec![],
                resume_from: OrchestrationState::Repairing,
            })
        }
    }
    struct NopChecks;
    impl CheckRunner for NopChecks {
        fn run(&self, check: &VerificationCheck) -> CheckResult {
            CheckResult {
                check_id: check.id.clone(),
                status: agent_runtime::orchestration::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                stdout_ref: None,
                stderr_ref: None,
                evidence_id: EvidenceId::new(),
                failure_summary: None,
            }
        }
    }

    fn contract(id: GoalId) -> TaskContract {
        TaskContract {
            id,
            parent_task_id: None,
            objective: "graph-backed".into(),
            scope: vec![],
            constraints: vec![],
            requirements: vec![RequirementNode {
                id: "REQ-001".into(),
                description: "host accept".into(),
                source: "t".into(),
                priority: RequirementPriority::Normal,
                mandatory: true,
                depends_on: vec![],
                acceptance_criteria: vec!["AC-001".into()],
            }],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-001".into(),
                text: "accepted".into(),
            }],
            permitted_capabilities: vec![],
            required_capabilities: vec![],
            forbidden_actions: vec![],
            expected_outputs: vec![],
            verification_policy: VerificationPolicy::for_complexity(TaskComplexity::Substantial),
            resource_budget: OrchestrationBudget::default(),
            context_budget: 1000,
            workspace_policy: WorkspacePolicy::Isolated,
            complexity: TaskComplexity::Substantial,
            metadata: BTreeSet::new(),
        }
    }

    fn drivers(id: GoalId, ev: EvidenceId) -> SupervisorDrivers {
        SupervisorDrivers {
            planner: Box::new(NopPlanner),
            explorer: Box::new(NopExplore),
            retriever: Box::new(NopExplore),
            implementer: Box::new(NopImpl { id, ev }),
            verifiers: vec![Box::new(NopVer { id, ev })],
            strategist: Box::new(NopStrat),
            checks: Box::new(NopChecks),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rapidlm-gbr-{name}-{}", SessionId::new()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drive the supervisor from `Contracting` to `Verified` with the Nop
    /// drivers: the recorded evidence node is what the candidate cites.
    fn verify_through(run: &mut GraphBackedRun, id: GoalId, ev: EvidenceId) {
        while run.orchestration_state() != OrchestrationState::Implementing {
            run.supervisor.advance().expect("advance");
        }
        run.supervisor
            .record_evidence(EvidenceNode {
                id: ev,
                task_id: id,
                requirement_ids: vec!["REQ-001".into()],
                producer: "host".into(),
                kind: agent_runtime::orchestration::OrchestrationEvidenceKind::Tool,
                source: "check".into(),
                timestamp: 1,
                artifact_ref: None,
                command_ref: Some("true".into()),
                workspace_ref: None,
                content_hash: None,
                summary: "passed".into(),
                structured_payload: None,
                trust_level: agent_runtime::orchestration::EvidenceTrust::Deterministic,
            })
            .expect("evidence");
        run.supervisor.advance().expect("implement");
        run.supervisor.run_checks().expect("checks");
        run.supervisor.verify().expect("verify");
        assert_eq!(run.orchestration_state(), OrchestrationState::Verified);
    }

    fn state_changes(run: &GraphBackedRun, session: SessionId) -> Vec<(String, String)> {
        let cancel = event_ledger::ledger::CancellationToken::new();
        let last = run.graphs.last_seq().unwrap();
        (1..=last)
            .filter_map(|seq| run.graphs.ledger().get(session, seq, &cancel).ok())
            .filter(|event| event.kind() == EventKind::GraphNodeStateChanged)
            .map(|event| {
                let payload = event.payload();
                (
                    payload["node_id"].as_str().unwrap_or("").to_owned(),
                    payload["state"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn graph_backed_run_creates_goal_task_verify_nodes_on_ledger() {
        let dir = scratch("default");
        let ledger = EventLedger::open(dir.join("l.db")).unwrap();
        let session = SessionId::new();
        let id = GoalId::new();
        let ev = EvidenceId::new();
        let mut run = GraphBackedRun::start(
            contract(id),
            WorkspaceIdentity::new("sha256:aa"),
            drivers(id, ev),
            ledger,
            session,
            ProjectId::new(),
        )
        .expect("start");
        assert_eq!(run.orchestration_state(), OrchestrationState::Contracting);
        let snap = run.graphs.snapshot(run.graph_id).unwrap();
        assert_eq!(snap.nodes.len(), 3);
        assert_eq!(snap.nodes[&run.goal_node].kind, NodeKind::Goal);
        assert_eq!(snap.nodes[&run.task_nodes[0]].kind, NodeKind::Task);
        assert_eq!(
            snap.nodes[&run.verify_nodes[0]].kind,
            NodeKind::Verification
        );
        let cancel = event_ledger::ledger::CancellationToken::new();
        let e1 = run.graphs.ledger().get(session, 1, &cancel).unwrap();
        assert_eq!(e1.kind(), EventKind::GraphCreated);
        let e2 = run.graphs.ledger().get(session, 2, &cancel).unwrap();
        assert_eq!(e2.kind(), EventKind::GraphRevisionCommitted);
        assert!(run.supervisor.accept().is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_planned_run_proposes_the_plan_under_the_goal_and_gates_on_its_edges() {
        let dir = scratch("planned");
        let ledger = EventLedger::open(dir.join("l.db")).unwrap();
        let session = SessionId::new();
        let id = GoalId::new();
        let ev = EvidenceId::new();
        let build = NodeId::new();
        let test = NodeId::new();
        let check = NodeId::new();
        let plan = RunPlan {
            nodes: vec![
                NodeSpec::new(build, NodeKind::Task, "build"),
                NodeSpec::new(test, NodeKind::Task, "test").with_max_attempts(1),
                NodeSpec::new(check, NodeKind::Verification, "check"),
            ],
            edges: vec![
                EdgeSpec {
                    from: build,
                    to: test,
                    kind: EdgeKind::DependsOn,
                    condition: crate::graph::EdgeCondition::default(),
                },
                EdgeSpec {
                    from: test,
                    to: check,
                    kind: EdgeKind::DependsOn,
                    condition: crate::graph::EdgeCondition::default(),
                },
            ],
        };
        let mut run = GraphBackedRun::start_planned(
            contract(id),
            WorkspaceIdentity::new("sha256:aa"),
            drivers(id, ev),
            ledger,
            session,
            ProjectId::new(),
            plan,
        )
        .expect("start");
        assert_eq!(run.task_nodes, vec![build, test]);
        assert_eq!(run.verify_nodes, vec![check]);
        let snap = run.graphs.snapshot(run.graph_id).unwrap();
        assert_eq!(snap.nodes.len(), 4);
        // Only the entry node hangs off the goal; the rest follow plan edges.
        let from_goal: Vec<NodeId> = snap
            .edges
            .iter()
            .filter(|e| e.from == run.goal_node)
            .map(|e| e.to)
            .collect();
        assert_eq!(from_goal, vec![build]);
        let ready = snap.ready_set();
        assert!(ready.contains(&build), "the entry step is ready");
        assert!(
            !ready.contains(&test) && !ready.contains(&check),
            "dependents wait: {ready:?}"
        );
        assert_eq!(
            snap.nodes[&test].max_attempts, 1,
            "the plan's retry ceiling"
        );
        assert_eq!(snap.nodes[&build].max_attempts, 3);

        // The graph's retry ceiling is the plan's: one attempt means no retry.
        run.graphs
            .set_state(run.graph_id, build, NodeState::Succeeded)
            .unwrap();
        run.graphs
            .set_state(run.graph_id, test, NodeState::Running)
            .unwrap();
        run.graphs
            .set_state(run.graph_id, test, NodeState::Failed)
            .unwrap();
        assert_eq!(
            run.graphs.retry(run.graph_id, test),
            Err(GraphError::InvalidState)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn accept_refuses_while_a_verification_node_failed_and_skips_succeeded_ones() {
        let dir = scratch("accept");
        let ledger = EventLedger::open(dir.join("l.db")).unwrap();
        let session = SessionId::new();
        let id = GoalId::new();
        let ev = EvidenceId::new();
        let task = NodeId::new();
        let ran = NodeId::new();
        let failed = NodeId::new();
        let plan = RunPlan {
            nodes: vec![
                NodeSpec::new(task, NodeKind::Task, "task"),
                NodeSpec::new(ran, NodeKind::Verification, "ran"),
                NodeSpec::new(failed, NodeKind::Verification, "failed"),
            ],
            edges: vec![],
        };
        let mut run = GraphBackedRun::start_planned(
            contract(id),
            WorkspaceIdentity::new("sha256:aa"),
            drivers(id, ev),
            ledger,
            session,
            ProjectId::new(),
            plan,
        )
        .expect("start");
        verify_through(&mut run, id, ev);
        run.graphs
            .set_state(run.graph_id, ran, NodeState::Succeeded)
            .unwrap();
        run.graphs
            .set_state(run.graph_id, failed, NodeState::Failed)
            .unwrap();
        let before = state_changes(&run, session).len();
        assert_eq!(run.accept(), Err(SupervisorError::MissingEvidence));
        assert_eq!(
            run.orchestration_state(),
            OrchestrationState::Verified,
            "the supervisor was not touched"
        );
        assert_eq!(
            state_changes(&run, session).len(),
            before,
            "nothing appended"
        );
        let snap = run.graphs.snapshot(run.graph_id).unwrap();
        assert_eq!(snap.nodes[&run.goal_node].state, NodeState::Pending);
        assert_eq!(snap.nodes[&failed].state, NodeState::Failed);

        // Once the failed check is re-run and passes, acceptance marks the
        // goal — and only the goal: the succeeded checks are not re-stated.
        run.graphs.retry(run.graph_id, failed).unwrap();
        run.graphs
            .set_state(run.graph_id, failed, NodeState::Succeeded)
            .unwrap();
        let before = state_changes(&run, session).len();
        run.accept().expect("accept");
        assert_eq!(run.orchestration_state(), OrchestrationState::Accepted);
        let appended: Vec<(String, String)> = state_changes(&run, session)
            .into_iter()
            .skip(before)
            .collect();
        assert_eq!(
            appended,
            vec![(run.goal_node.to_string(), "succeeded".to_owned())]
        );
        let _ = fs::remove_dir_all(dir);
    }
}
