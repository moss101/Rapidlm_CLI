//! Host GraphService. Ledger append happens before in-memory apply.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use event_ledger::event::{ActorKind, ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger};
use protocol::{EventId, GraphId, NodeId, ProjectId, RedactionClass, SessionId, TraceId};
use serde_json::json;

use crate::graph::{Node, RuntimeGraph};
use crate::kinds::{NodeKind, NodeState};
use crate::proposal::{GraphProposal, ProposalError, validate_and_apply};

pub struct GraphService {
    ledger: EventLedger,
    session: SessionId,
    graphs: BTreeMap<GraphId, RuntimeGraph>,
    history: BTreeMap<GraphId, Vec<RuntimeGraph>>,
    fair_cursor: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub enum GraphError {
    Ledger(String),
    UnknownGraph,
    Proposal(ProposalError),
    InvalidState,
}

impl GraphService {
    pub fn open(
        ledger: EventLedger,
        session: SessionId,
        project: ProjectId,
    ) -> Result<Self, GraphError> {
        let cancel = event_ledger::ledger::CancellationToken::new();
        match ledger.create_session(session, project, &cancel) {
            Ok(()) | Err(event_ledger::ledger::LedgerError::SessionExists { .. }) => {}
            Err(err) => return Err(GraphError::Ledger(err.to_string())),
        }
        Ok(Self {
            ledger,
            session,
            graphs: BTreeMap::new(),
            history: BTreeMap::new(),
            fair_cursor: 0,
        })
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Create a graph with a Goal root. Appends `graph.created` before the snapshot exists.
    pub fn create(&mut self, label: &str) -> Result<RuntimeGraph, GraphError> {
        let graph_id = GraphId::new();
        let root = NodeId::new();
        let graph = RuntimeGraph {
            graph_id,
            revision: 1,
            root,
            nodes: BTreeMap::from([(root, Node::new(root, NodeKind::Goal, label))]),
            edges: Vec::new(),
        };
        self.append(
            EventKind::GraphCreated,
            json!({
                "graph_id": graph_id.to_string(),
                "revision": 1,
                "root": root.to_string(),
            }),
        )?;
        self.graphs.insert(graph_id, graph.clone());
        self.history
            .entry(graph_id)
            .or_default()
            .push(graph.clone());
        Ok(graph)
    }

    /// Host-validated proposal. Models cannot call this as an authority.
    pub fn propose(
        &mut self,
        graph_id: GraphId,
        proposal: GraphProposal,
    ) -> Result<RuntimeGraph, GraphError> {
        let current = self.graphs.get(&graph_id).ok_or(GraphError::UnknownGraph)?;
        match validate_and_apply(current, &proposal) {
            Ok(next) => {
                self.append(
                    EventKind::GraphRevisionCommitted,
                    json!({
                        "graph_id": graph_id.to_string(),
                        "revision": next.revision,
                        "base_revision": proposal.base_revision,
                    }),
                )?;
                self.graphs.insert(graph_id, next.clone());
                self.history.entry(graph_id).or_default().push(next.clone());
                Ok(next)
            }
            Err(err) => {
                let _ = self.append(
                    EventKind::GraphProposalRejected,
                    json!({
                        "graph_id": graph_id.to_string(),
                        "reason": format!("{err:?}"),
                    }),
                );
                Err(GraphError::Proposal(err))
            }
        }
    }

    pub fn snapshot(&self, graph_id: GraphId) -> Result<&RuntimeGraph, GraphError> {
        self.graphs.get(&graph_id).ok_or(GraphError::UnknownGraph)
    }

    pub fn last_seq(&self) -> Result<u64, GraphError> {
        let cancel = event_ledger::ledger::CancellationToken::new();
        self.ledger
            .last_seq(self.session, &cancel)
            .map_err(|e| GraphError::Ledger(e.to_string()))
    }

    /// Host-only state change. Ledger append precedes the in-memory mutation.
    ///
    /// Entering `Running` from `Pending`/`Ready` is an attempt and counts
    /// against the node's `max_attempts`; resuming a `Paused` node is the
    /// same attempt continuing.
    pub fn set_state(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
        state: NodeState,
    ) -> Result<RuntimeGraph, GraphError> {
        let graph = self.graphs.get(&graph_id).ok_or(GraphError::UnknownGraph)?;
        let Some(node) = graph.node(node_id) else {
            return Err(GraphError::Proposal(ProposalError::UnknownNode));
        };
        let new_attempt = state == NodeState::Running
            && matches!(node.state, NodeState::Pending | NodeState::Ready);
        let attempts = if new_attempt {
            node.attempts.saturating_add(1)
        } else {
            node.attempts
        };
        self.append(
            EventKind::GraphNodeStateChanged,
            json!({
                "graph_id": graph_id.to_string(),
                "node_id": node_id.to_string(),
                "state": state.as_str(),
                "attempts": attempts,
            }),
        )?;
        let graph = self
            .graphs
            .get_mut(&graph_id)
            .ok_or(GraphError::UnknownGraph)?;
        let node = graph.nodes.get_mut(&node_id).expect("checked");
        node.state = state;
        node.attempts = attempts;
        graph.revision = graph.revision.saturating_add(1);
        let out = graph.clone();
        self.history.entry(graph_id).or_default().push(out.clone());
        Ok(out)
    }

    pub fn diff(
        &self,
        graph_id: GraphId,
        from: u64,
        to: u64,
    ) -> Result<crate::graph::GraphDiff, GraphError> {
        let hist = self
            .history
            .get(&graph_id)
            .ok_or(GraphError::UnknownGraph)?;
        let older = hist
            .iter()
            .find(|g| g.revision == from)
            .ok_or(GraphError::InvalidState)?;
        let newer = hist
            .iter()
            .find(|g| g.revision == to)
            .ok_or(GraphError::InvalidState)?;
        Ok(newer.diff(older))
    }

    pub fn cancel_tree(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
    ) -> Result<RuntimeGraph, GraphError> {
        let descendants = self.descendants(graph_id, node_id)?;
        self.set_state(graph_id, node_id, NodeState::Cancelled)?;
        for id in descendants {
            let state = self
                .graphs
                .get(&graph_id)
                .and_then(|g| g.node(id))
                .map(|n| n.state);
            if state.is_some_and(|s| !s.is_terminal()) {
                self.set_state(graph_id, id, NodeState::Cancelled)?;
            }
        }
        self.snapshot(graph_id).cloned()
    }

    /// Re-queue a failed node while an attempt remains. `attempts` counts
    /// runs (every entry into `Running`), so a node whose `max_attempts` is
    /// 1 never retries; the retry itself is not an attempt — the `Running`
    /// that follows is.
    pub fn retry(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
    ) -> Result<RuntimeGraph, GraphError> {
        let node = self
            .graphs
            .get(&graph_id)
            .and_then(|g| g.node(node_id))
            .cloned()
            .ok_or(GraphError::Proposal(ProposalError::UnknownNode))?;
        if node.state != NodeState::Failed {
            return Err(GraphError::InvalidState);
        }
        if node.attempts >= node.max_attempts {
            return Err(GraphError::InvalidState);
        }
        self.append(
            EventKind::GraphNodeStateChanged,
            json!({
                "graph_id": graph_id.to_string(),
                "node_id": node_id.to_string(),
                "state": "pending",
                "retry": true,
                "attempts": node.attempts,
            }),
        )?;
        let graph = self
            .graphs
            .get_mut(&graph_id)
            .ok_or(GraphError::UnknownGraph)?;
        let n = graph.nodes.get_mut(&node_id).expect("checked");
        n.state = NodeState::Pending;
        graph.revision = graph.revision.saturating_add(1);
        let out = graph.clone();
        self.history.entry(graph_id).or_default().push(out.clone());
        Ok(out)
    }

    pub fn wait(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
        token: &str,
    ) -> Result<RuntimeGraph, GraphError> {
        {
            let n = self
                .graphs
                .get_mut(&graph_id)
                .and_then(|g| g.nodes.get_mut(&node_id))
                .ok_or(GraphError::Proposal(ProposalError::UnknownNode))?;
            n.wait_token = Some(token.to_owned());
        }
        self.set_state(graph_id, node_id, NodeState::Waiting)
    }

    pub fn resume_wait(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
    ) -> Result<RuntimeGraph, GraphError> {
        let token_ok = self
            .graphs
            .get(&graph_id)
            .and_then(|g| g.node(node_id))
            .is_some_and(|n| n.state == NodeState::Waiting && n.wait_token.is_some());
        if !token_ok {
            return Err(GraphError::InvalidState);
        }
        self.set_state(graph_id, node_id, NodeState::Pending)
    }

    /// Suspend a running node without cancelling it: the process/agent step
    /// underneath keeps its progress and is resumable, distinct from both
    /// `cancel_tree` (terminal, never resumes) and `wait` (blocked on an
    /// external token, re-enters scheduling via `Pending` on resume). Only a
    /// currently-`Running` node can be paused; the scheduler never enters
    /// `Paused` on its own.
    pub fn pause(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
    ) -> Result<RuntimeGraph, GraphError> {
        let running = self
            .graphs
            .get(&graph_id)
            .and_then(|g| g.node(node_id))
            .is_some_and(|n| n.state == NodeState::Running);
        if !running {
            return Err(GraphError::InvalidState);
        }
        self.set_state(graph_id, node_id, NodeState::Paused)
    }

    /// Resume a paused node directly back to `Running`: unlike `resume_wait`,
    /// there is no dependency to re-check — the node was already running and
    /// mid-work when it was paused, so it picks up exactly where it left off
    /// rather than re-entering the ready queue.
    pub fn resume(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
    ) -> Result<RuntimeGraph, GraphError> {
        let paused = self
            .graphs
            .get(&graph_id)
            .and_then(|g| g.node(node_id))
            .is_some_and(|n| n.state == NodeState::Paused);
        if !paused {
            return Err(GraphError::InvalidState);
        }
        self.set_state(graph_id, node_id, NodeState::Running)
    }

    /// Bounded invalidation: the node and descendants up to `bound` hops.
    pub fn invalidate_from(
        &mut self,
        graph_id: GraphId,
        node_id: NodeId,
        bound: usize,
    ) -> Result<RuntimeGraph, GraphError> {
        let mut wave = vec![node_id];
        let mut seen = BTreeMap::new();
        seen.insert(node_id, 0usize);
        let mut i = 0;
        while i < wave.len() {
            let cur = wave[i];
            let depth = seen[&cur];
            if depth < bound {
                for child in self.successors(graph_id, cur)? {
                    if let std::collections::btree_map::Entry::Vacant(e) = seen.entry(child) {
                        e.insert(depth + 1);
                        wave.push(child);
                    }
                }
            }
            i += 1;
        }
        for id in wave {
            let st = self
                .graphs
                .get(&graph_id)
                .and_then(|g| g.node(id))
                .map(|n| n.state);
            if st.is_some_and(|s| !matches!(s, NodeState::Cancelled)) {
                self.set_state(graph_id, id, NodeState::Invalidated)?;
            }
        }
        self.snapshot(graph_id).cloned()
    }

    pub fn fan_out(
        &mut self,
        graph_id: GraphId,
        parent: NodeId,
        shards: u32,
    ) -> Result<Vec<NodeId>, GraphError> {
        if shards == 0 || shards > 32 {
            return Err(GraphError::InvalidState);
        }
        let rev = self.snapshot(graph_id)?.revision;
        let mut ids = Vec::new();
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..shards {
            let id = NodeId::new();
            ids.push(id);
            nodes.push(crate::proposal::NodeSpec::new(
                id,
                NodeKind::Task,
                format!("shard-{i}"),
            ));
            edges.push(crate::proposal::EdgeSpec {
                from: parent,
                to: id,
                kind: crate::kinds::EdgeKind::DecomposesInto,
                condition: crate::graph::EdgeCondition::default(),
            });
        }
        self.propose(
            graph_id,
            crate::proposal::GraphProposal {
                base_revision: rev,
                add_nodes: nodes,
                add_edges: edges,
                supersede: vec![],
                invalidate: vec![],
            },
        )?;
        Ok(ids)
    }

    pub fn next_fair_ready(&mut self) -> Option<(GraphId, NodeId)> {
        if self.graphs.is_empty() {
            return None;
        }
        let ids: Vec<GraphId> = self.graphs.keys().copied().collect();
        let n = ids.len();
        for i in 0..n {
            let idx = (self.fair_cursor + i) % n;
            let gid = ids[idx];
            if let Some(ready) = self.graphs[&gid].ready_set().first().copied() {
                self.fair_cursor = (idx + 1) % n;
                return Some((gid, ready));
            }
        }
        None
    }

    pub fn export(&self, graph_id: GraphId, format: &str) -> Result<String, GraphError> {
        let g = self.snapshot(graph_id)?;
        match format {
            "json" => g
                .export_json()
                .map_err(|e| GraphError::Ledger(e.to_string())),
            "dot" => Ok(g.export_dot()),
            "mermaid" => Ok(g.export_mermaid()),
            _ => Err(GraphError::InvalidState),
        }
    }

    pub fn inspect(
        &self,
        graph_id: GraphId,
        node: NodeId,
    ) -> Result<crate::graph::NodeExplain, GraphError> {
        self.inspect_with(graph_id, node, &crate::graph::ReadyContext::default())
    }

    pub fn inspect_with(
        &self,
        graph_id: GraphId,
        node: NodeId,
        ctx: &crate::graph::ReadyContext,
    ) -> Result<crate::graph::NodeExplain, GraphError> {
        let g = self.snapshot(graph_id)?;
        let n = g
            .node(node)
            .ok_or(GraphError::Proposal(ProposalError::UnknownNode))?;
        let (ready, reasons) = g.is_ready(node, ctx);
        Ok(crate::graph::NodeExplain {
            node_id: node,
            state: n.state,
            ready,
            reasons,
        })
    }

    fn descendants(&self, graph_id: GraphId, node_id: NodeId) -> Result<Vec<NodeId>, GraphError> {
        let mut out = Vec::new();
        let mut stack = self.successors(graph_id, node_id)?;
        while let Some(id) = stack.pop() {
            if !out.contains(&id) {
                out.push(id);
                stack.extend(self.successors(graph_id, id)?);
            }
        }
        Ok(out)
    }

    fn successors(&self, graph_id: GraphId, node_id: NodeId) -> Result<Vec<NodeId>, GraphError> {
        let g = self.graphs.get(&graph_id).ok_or(GraphError::UnknownGraph)?;
        Ok(g.edges
            .iter()
            .filter(|e| e.from == node_id)
            .map(|e| e.to)
            .collect())
    }

    fn append(&mut self, kind: EventKind, payload: serde_json::Value) -> Result<(), GraphError> {
        let actor = ActorRef::new(ActorKind::System, &EventId::new().to_string())
            .map_err(|e| GraphError::Ledger(e.to_string()))?;
        let options = AppendOptions {
            redaction: RedactionClass::Public,
            trace_id: TraceId::new(),
            expected_seq: None,
        };
        let cancel = event_ledger::ledger::CancellationToken::new();
        self.ledger
            .append(self.session, actor, kind, payload, &options, &cancel)
            .map_err(|e| GraphError::Ledger(e.to_string()))?;
        Ok(())
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(s) => write!(f, "ledger: {s}"),
            Self::UnknownGraph => f.write_str("unknown graph"),
            Self::Proposal(p) => write!(f, "proposal rejected: {p:?}"),
            Self::InvalidState => f.write_str("invalid graph state"),
        }
    }
}

impl Error for GraphError {}
