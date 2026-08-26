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
use crate::proposal::{EdgeSpec, GraphProposal, NodeSpec};
use crate::service::{GraphError, GraphService};

/// Graph-backed run: Goal/Task/Verification nodes plus the existing Supervisor.
pub struct GraphBackedRun {
    pub graphs: GraphService,
    pub graph_id: GraphId,
    pub goal_node: NodeId,
    pub task_node: NodeId,
    pub verify_node: NodeId,
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
        let mut graphs = GraphService::open(ledger, session, project)?;
        let created = graphs.create(&contract.objective)?;
        let graph_id = created.graph_id;
        let goal_node = created.root;
        let task_node = NodeId::new();
        let verify_node = NodeId::new();
        graphs.propose(
            graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![
                    NodeSpec {
                        id: task_node,
                        kind: NodeKind::Task,
                        label: "implement".into(),
                        workspace_key: None,
                        resource_key: None,
                        budget_tokens: 0,
                    },
                    NodeSpec {
                        id: verify_node,
                        kind: NodeKind::Verification,
                        label: "verify".into(),
                        workspace_key: None,
                        resource_key: None,
                        budget_tokens: 0,
                    },
                ],
                add_edges: vec![
                    EdgeSpec {
                        from: goal_node,
                        to: task_node,
                        kind: EdgeKind::DecomposesInto,
                        condition: crate::graph::EdgeCondition::default(),
                    },
                    EdgeSpec {
                        from: task_node,
                        to: verify_node,
                        kind: EdgeKind::DependsOn,
                        condition: crate::graph::EdgeCondition::default(),
                    },
                ],
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
            task_node,
            verify_node,
            supervisor,
        })
    }

    pub fn accept(&mut self) -> Result<(), SupervisorError> {
        self.supervisor.accept()?;
        self.graphs
            .set_state(self.graph_id, self.goal_node, NodeState::Succeeded)
            .map_err(|e| SupervisorError::Sink(e.to_string()))?;
        self.graphs
            .set_state(self.graph_id, self.verify_node, NodeState::Succeeded)
            .map_err(|e| SupervisorError::Sink(e.to_string()))?;
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

    #[test]
    fn graph_backed_run_creates_goal_task_verify_nodes_on_ledger() {
        let dir = std::env::temp_dir().join(format!("rapidlm-gbr-{}", SessionId::new()));
        fs::create_dir_all(&dir).unwrap();
        let ledger = EventLedger::open(dir.join("l.db")).unwrap();
        let session = SessionId::new();
        let id = GoalId::new();
        let ev = EvidenceId::new();
        let drivers = SupervisorDrivers {
            planner: Box::new(NopPlanner),
            explorer: Box::new(NopExplore),
            retriever: Box::new(NopExplore),
            implementer: Box::new(NopImpl { id, ev }),
            verifiers: vec![Box::new(NopVer { id, ev })],
            strategist: Box::new(NopStrat),
            checks: Box::new(NopChecks),
        };
        let mut run = GraphBackedRun::start(
            contract(id),
            WorkspaceIdentity::new("sha256:aa"),
            drivers,
            ledger,
            session,
            ProjectId::new(),
        )
        .expect("start");
        assert_eq!(run.orchestration_state(), OrchestrationState::Contracting);
        let snap = run.graphs.snapshot(run.graph_id).unwrap();
        assert_eq!(snap.nodes.len(), 3);
        assert_eq!(snap.nodes[&run.goal_node].kind, NodeKind::Goal);
        assert_eq!(snap.nodes[&run.task_node].kind, NodeKind::Task);
        assert_eq!(snap.nodes[&run.verify_node].kind, NodeKind::Verification);
        let cancel = event_ledger::ledger::CancellationToken::new();
        let e1 = run.graphs.ledger().get(session, 1, &cancel).unwrap();
        assert_eq!(e1.kind(), EventKind::GraphCreated);
        let e2 = run.graphs.ledger().get(session, 2, &cancel).unwrap();
        assert_eq!(e2.kind(), EventKind::GraphRevisionCommitted);
        assert!(run.supervisor.accept().is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
