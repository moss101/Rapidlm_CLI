//! Host-owned Dynamic Runtime Graph and scheduler.
//!
//! Durable transitions append to the existing Event Ledger before in-memory
//! apply. The verified-orchestration [`agent_runtime::Supervisor`] is KEPT and
//! adapted through [`orch::GraphBackedRun`].

#![forbid(unsafe_code)]

pub mod graph;
pub mod cron;
pub mod playbook;
pub mod kinds;
pub mod orch;
pub mod proposal;
pub mod service;

pub use cron::{
    CronError, DuePrompt, PollReport, PromptCron, CRON_FACADE_SCHEMA, FIRING_LEASE_TIMEOUT_MS,
    MAX_POLL_BATCH, QUARANTINE_UNPARSEABLE_SCHEDULE,
};

pub use graph::{Edge, EdgeCondition, GraphDiff, Node, NodeExplain, ReadyContext, RuntimeGraph};
pub use kinds::{EdgeKind, NodeKind, NodeState};
pub use orch::GraphBackedRun;
pub use proposal::{EdgeSpec, GraphProposal, NodeSpec, ProposalError};
pub use service::{GraphError, GraphService};

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::EventKind;
    use event_ledger::ledger::EventLedger;
    use protocol::{NodeId, ProjectId, SessionId};
    use std::fs;

    fn open_service() -> (GraphService, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rapidlm-graph-{}", SessionId::new()));
        fs::create_dir_all(&dir).expect("dir");
        let ledger = EventLedger::open(dir.join("ledger.db")).expect("ledger");
        let svc = GraphService::open(ledger, SessionId::new(), ProjectId::new()).expect("open");
        (svc, dir)
    }

    #[test]
    fn create_appends_graph_created_before_snapshot_is_readable() {
        let (mut svc, dir) = open_service();
        let session = svc.session();
        let graph = svc.create("ship v3 graph").expect("create");
        assert_eq!(graph.revision, 1);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[&graph.root].kind, NodeKind::Goal);
        let cancel = event_ledger::ledger::CancellationToken::new();
        let seq = svc.last_seq().expect("seq");
        assert_eq!(seq, 1);
        let event = svc.ledger().get(session, 1, &cancel).expect("get");
        assert_eq!(event.kind(), EventKind::GraphCreated);
        let loaded = svc.snapshot(graph.graph_id).expect("snapshot");
        assert_eq!(loaded.graph_id, graph.graph_id);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn host_proposal_commits_revision_to_ledger() {
        let (mut svc, dir) = open_service();
        let session = svc.session();
        let graph = svc.create("root").expect("create");
        let task = NodeId::new();
        let next = svc
            .propose(
                graph.graph_id,
                GraphProposal {
                    base_revision: 1,
                    add_nodes: vec![NodeSpec {
                        id: task,
                        kind: NodeKind::Task,
                        label: "implement".into(),
                        workspace_key: None,
                        resource_key: None,
                        budget_tokens: 0,
                    }],
                    add_edges: vec![EdgeSpec {
                        from: graph.root,
                        to: task,
                        kind: EdgeKind::DecomposesInto,
                        condition: EdgeCondition::default(),
                    }],
                    supersede: vec![],
                    invalidate: vec![],
                },
            )
            .expect("propose");
        assert_eq!(next.revision, 2);
        assert!(next.nodes.contains_key(&task));
        let cancel = event_ledger::ledger::CancellationToken::new();
        let event = svc.ledger().get(session, 2, &cancel).expect("get");
        assert_eq!(event.kind(), EventKind::GraphRevisionCommitted);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cycle_and_stale_revision_fail_closed_without_applying() {
        let (mut svc, dir) = open_service();
        let graph = svc.create("root").expect("create");
        let a = NodeId::new();
        let b = NodeId::new();
        svc.propose(
            graph.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![
                    NodeSpec {
                        id: a,
                        kind: NodeKind::Task,
                        label: "a".into(),
                        workspace_key: None,
                        resource_key: None,
                        budget_tokens: 0,
                    },
                    NodeSpec {
                        id: b,
                        kind: NodeKind::Task,
                        label: "b".into(),
                        workspace_key: None,
                        resource_key: None,
                        budget_tokens: 0,
                    },
                ],
                add_edges: vec![
                    EdgeSpec {
                        from: a,
                        to: b,
                        kind: EdgeKind::DependsOn,
                        condition: EdgeCondition::default(),
                    },
                    EdgeSpec {
                        from: b,
                        to: a,
                        kind: EdgeKind::DependsOn,
                        condition: EdgeCondition::default(),
                    },
                ],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .expect_err("cycle");
        let snap = svc.snapshot(graph.graph_id).expect("snap");
        assert_eq!(snap.revision, 1);
        assert_eq!(snap.nodes.len(), 1);
        svc.propose(
            graph.graph_id,
            GraphProposal {
                base_revision: 99,
                add_nodes: vec![],
                add_edges: vec![],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .expect_err("stale");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ready_set_is_deterministic_and_depends_on_success() {
        let (mut svc, dir) = open_service();
        let graph = svc.create("root").expect("create");
        let t1 = NodeId::new();
        let t2 = NodeId::new();
        let next = svc
            .propose(
                graph.graph_id,
                GraphProposal {
                    base_revision: 1,
                    add_nodes: vec![
                        NodeSpec {
                            id: t1,
                            kind: NodeKind::Task,
                            label: "t1".into(),
                            workspace_key: None,
                            resource_key: None,
                            budget_tokens: 0,
                        },
                        NodeSpec {
                            id: t2,
                            kind: NodeKind::Task,
                            label: "t2".into(),
                            workspace_key: None,
                            resource_key: None,
                            budget_tokens: 0,
                        },
                    ],
                    add_edges: vec![EdgeSpec {
                        from: t1,
                        to: t2,
                        kind: EdgeKind::DependsOn,
                        condition: EdgeCondition::default(),
                    }],
                    supersede: vec![],
                    invalidate: vec![],
                },
            )
            .expect("propose");
        let ready = next.ready_set();
        assert!(ready.contains(&t1));
        assert!(!ready.contains(&t2));
        svc.set_state(graph.graph_id, t1, NodeState::Succeeded)
            .expect("state");
        let ready2 = svc.snapshot(graph.graph_id).unwrap().ready_set();
        assert!(ready2.contains(&t2));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn set_state_appends_before_memory_update() {
        let (mut svc, dir) = open_service();
        let session = svc.session();
        let graph = svc.create("root").expect("create");
        svc.set_state(graph.graph_id, graph.root, NodeState::Succeeded)
            .expect("set");
        let cancel = event_ledger::ledger::CancellationToken::new();
        let seq = svc.last_seq().unwrap();
        let event = svc.ledger().get(session, seq, &cancel).expect("get");
        assert_eq!(event.kind(), EventKind::GraphNodeStateChanged);
        assert_eq!(
            svc.snapshot(graph.graph_id).unwrap().nodes[&graph.root].state,
            NodeState::Succeeded
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn immutable_diff_records_added_and_state_changes() {
        let (mut svc, dir) = open_service();
        let graph = svc.create("root").expect("create");
        let task = NodeId::new();
        svc.propose(
            graph.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![NodeSpec {
                    id: task,
                    kind: NodeKind::Task,
                    label: "t".into(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: 0,
                }],
                add_edges: vec![],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        let diff = svc.diff(graph.graph_id, 1, 2).expect("diff");
        assert_eq!(diff.from_revision, 1);
        assert_eq!(diff.to_revision, 2);
        assert!(diff.nodes_added.contains(&task));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn join_ready_requires_all_predecessors_and_fanout_creates_shards() {
        let (mut svc, dir) = open_service();
        let graph = svc.create("root").expect("create");
        let shards = svc.fan_out(graph.graph_id, graph.root, 2).expect("fanout");
        assert_eq!(shards.len(), 2);
        let join = NodeId::new();
        let rev = svc.snapshot(graph.graph_id).unwrap().revision;
        svc.propose(
            graph.graph_id,
            GraphProposal {
                base_revision: rev,
                add_nodes: vec![NodeSpec {
                    id: join,
                    kind: NodeKind::Join,
                    label: "join".into(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: 0,
                }],
                add_edges: vec![
                    EdgeSpec {
                        from: shards[0],
                        to: join,
                        kind: EdgeKind::JoinsAt,
                        condition: EdgeCondition::default(),
                    },
                    EdgeSpec {
                        from: shards[1],
                        to: join,
                        kind: EdgeKind::JoinsAt,
                        condition: EdgeCondition::default(),
                    },
                ],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        let ready = svc.snapshot(graph.graph_id).unwrap().ready_set();
        assert!(!ready.contains(&join));
        svc.set_state(graph.graph_id, shards[0], NodeState::Succeeded)
            .unwrap();
        svc.set_state(graph.graph_id, shards[1], NodeState::Succeeded)
            .unwrap();
        let ready = svc.snapshot(graph.graph_id).unwrap().ready_set();
        assert!(ready.contains(&join));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resource_and_workspace_conflicts_block_ready() {
        let (mut svc, dir) = open_service();
        let graph = svc.create("root").expect("create");
        let a = NodeId::new();
        svc.propose(
            graph.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![NodeSpec {
                    id: a,
                    kind: NodeKind::Task,
                    label: "writer".into(),
                    workspace_key: Some("ws".into()),
                    resource_key: Some("cpu".into()),
                    budget_tokens: 0,
                }],
                add_edges: vec![],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        let ctx = ReadyContext {
            busy_resources: ["cpu".into()].into_iter().collect(),
            busy_workspaces: ["ws".into()].into_iter().collect(),
        };
        let ready = svc.snapshot(graph.graph_id).unwrap().ready_set_with(&ctx);
        assert!(!ready.contains(&a));
        let explain = svc.inspect_with(graph.graph_id, a, &ctx).unwrap();
        assert!(!explain.ready);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_retry_wait_invalidate_export_and_fairness() {
        let (mut svc, dir) = open_service();
        let g1 = svc.create("g1").unwrap();
        let child = NodeId::new();
        svc.propose(
            g1.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![NodeSpec {
                    id: child,
                    kind: NodeKind::Task,
                    label: "c".into(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: 0,
                }],
                add_edges: vec![EdgeSpec {
                    from: g1.root,
                    to: child,
                    kind: EdgeKind::DecomposesInto,
                    condition: EdgeCondition::default(),
                }],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        svc.cancel_tree(g1.graph_id, g1.root).unwrap();
        assert_eq!(
            svc.snapshot(g1.graph_id).unwrap().nodes[&child].state,
            NodeState::Cancelled
        );

        let g2 = svc.create("g2").unwrap();
        svc.set_state(g2.graph_id, g2.root, NodeState::Failed)
            .unwrap();
        svc.retry(g2.graph_id, g2.root).unwrap();
        assert_eq!(
            svc.snapshot(g2.graph_id).unwrap().nodes[&g2.root].state,
            NodeState::Pending
        );
        assert_eq!(
            svc.snapshot(g2.graph_id).unwrap().nodes[&g2.root].attempts,
            1
        );

        svc.wait(g2.graph_id, g2.root, "tok-1").unwrap();
        assert_eq!(
            svc.snapshot(g2.graph_id).unwrap().nodes[&g2.root].state,
            NodeState::Waiting
        );
        svc.resume_wait(g2.graph_id, g2.root).unwrap();
        assert_eq!(
            svc.snapshot(g2.graph_id).unwrap().nodes[&g2.root].state,
            NodeState::Pending
        );

        let g3 = svc.create("g3").unwrap();
        let leaf = NodeId::new();
        svc.propose(
            g3.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![NodeSpec {
                    id: leaf,
                    kind: NodeKind::Task,
                    label: "leaf".into(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: 0,
                }],
                add_edges: vec![EdgeSpec {
                    from: g3.root,
                    to: leaf,
                    kind: EdgeKind::DependsOn,
                    condition: EdgeCondition::default(),
                }],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        svc.invalidate_from(g3.graph_id, g3.root, 2).unwrap();
        assert_eq!(
            svc.snapshot(g3.graph_id).unwrap().nodes[&leaf].state,
            NodeState::Invalidated
        );

        let json = svc.export(g3.graph_id, "json").unwrap();
        assert!(json.contains("revision"));
        let dot = svc.export(g3.graph_id, "dot").unwrap();
        assert!(dot.contains("digraph"));
        let mermaid = svc.export(g3.graph_id, "mermaid").unwrap();
        assert!(mermaid.contains("flowchart"));
        assert!(svc.next_fair_ready().is_some());

        let g4 = svc.create("g4").unwrap();
        let ask = NodeId::new();
        svc.propose(
            g4.graph_id,
            GraphProposal {
                base_revision: 1,
                add_nodes: vec![NodeSpec {
                    id: ask,
                    kind: NodeKind::AskUser,
                    label: "ask".into(),
                    workspace_key: None,
                    resource_key: None,
                    budget_tokens: 0,
                }],
                add_edges: vec![],
                supersede: vec![],
                invalidate: vec![],
            },
        )
        .unwrap();
        svc.wait(g4.graph_id, ask, "approval-1").unwrap();
        assert_eq!(
            svc.snapshot(g4.graph_id).unwrap().nodes[&ask].state,
            NodeState::Waiting
        );
        svc.resume_wait(g4.graph_id, ask).unwrap();
        assert_eq!(
            svc.snapshot(g4.graph_id).unwrap().nodes[&ask].state,
            NodeState::Pending
        );
        let _ = fs::remove_dir_all(dir);
    }
}
