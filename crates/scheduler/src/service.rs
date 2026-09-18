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

/// Outcome of [`GraphService::replay`]. `Rebuilt` carries the reconstructed
/// `graphs` map; `Unsupported` reports the first pre-payload event that
/// cannot be rebuilt, rather than returning a partial or guessed graph.
#[derive(Debug, Eq, PartialEq)]
pub enum GraphReplay {
    Rebuilt(BTreeMap<GraphId, RuntimeGraph>),
    Unsupported { first_seq: u64 },
}

/// Wire marker for [`AcceptanceEvent`]. `orchestration.task_accepted` is a
/// shared event kind — the supervisor's own `TaskAccepted` maps onto the
/// same string — so a reducer must be able to tell this record from another
/// producer's without guessing from a failed parse.
pub const ACCEPTANCE_RECORD: &str = "rapidlm.graph.acceptance/v1";

/// The one durable record an acceptance appends (GVS-006). It carries
/// everything both projections need: which supervisor run was accepted
/// (`task_id`, `candidate_digest`, `verdict`, `workspace_identity`) and
/// which graph nodes the acceptance completes (`goal_node`,
/// `verify_nodes`). Appended once, before either projection is touched, and
/// folded back into the graph by [`GraphService::replay`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AcceptanceEvent {
    /// Always [`ACCEPTANCE_RECORD`]; identifies this shape on a shared kind.
    pub record: String,
    pub graph_id: GraphId,
    pub goal_node: NodeId,
    pub verify_nodes: Vec<NodeId>,
    pub task_id: String,
    pub candidate_digest: String,
    /// The verdict in its own serde encoding, not a `Debug` rendering: this
    /// is a durable field a reader decodes back into a `Verdict`.
    pub verdict: agent_runtime::orchestration::Verdict,
    pub workspace_identity: String,
}

impl AcceptanceEvent {
    /// Every graph node this acceptance marks succeeded, goal first.
    pub fn completed_nodes(&self) -> Vec<NodeId> {
        let mut nodes = vec![self.goal_node];
        nodes.extend(self.verify_nodes.iter().copied());
        nodes
    }
}

fn parse_graph_id(payload: &serde_json::Value, seq: u64) -> Result<GraphId, GraphError> {
    payload
        .get("graph_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| GraphError::Ledger(format!("seq {seq}: bad graph_id")))
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
                // The whole graph, so a restart rebuilds it without guessing
                // (ADR 0021 §2). Serialization cannot fail for an owned graph.
                "graph": serde_json::to_value(&graph).unwrap_or(json!(null)),
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
                        // The validated proposal, re-applied deterministically
                        // on replay through the same `validate_and_apply`.
                        "proposal": serde_json::to_value(&proposal).unwrap_or(json!(null)),
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
    /// same attempt continuing. A node that went `Waiting` re-enters the
    /// queue through `Pending` (`resume_wait`), so its next `Running` is a
    /// new attempt — a wait ends the attempt it interrupted.
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
        // The wait token travels with the transition: it is part of the
        // node's durable state, and a replay that dropped it would rebuild
        // a waiting node that no longer knows what it waits on.
        let wait_token = node.wait_token.clone();
        self.append(
            EventKind::GraphNodeStateChanged,
            json!({
                "graph_id": graph_id.to_string(),
                "node_id": node_id.to_string(),
                "state": state.as_str(),
                "attempts": attempts,
                "wait_token": wait_token,
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

    /// Rebuild every graph in `session` from its durable events alone — the
    /// reducer ADR 0021 §2 requires, so a restart restores the same `graphs`
    /// map a live `GraphService` held. The session may carry non-graph
    /// events (session, run progress, orchestration); they are skipped.
    ///
    /// A `graph.created`/`graph.revision_committed` event written before this
    /// slice carried no `graph`/`proposal` payload and cannot be rebuilt;
    /// rather than guess, the whole replay returns
    /// [`GraphReplay::Unsupported`] naming the first such sequence. Nothing
    /// in production created graphs before payload-complete events, so no
    /// real history is affected — the variant exists so the reducer is
    /// honest by construction.
    pub fn replay(&self, session: SessionId) -> Result<GraphReplay, GraphError> {
        let cancel = event_ledger::ledger::CancellationToken::new();
        let last = self
            .ledger
            .last_seq(session, &cancel)
            .map_err(|e| GraphError::Ledger(e.to_string()))?;
        let mut graphs: BTreeMap<GraphId, RuntimeGraph> = BTreeMap::new();
        for seq in 1..=last {
            let envelope = match self.ledger.get(session, seq, &cancel) {
                Ok(envelope) => envelope,
                Err(err) => return Err(GraphError::Ledger(err.to_string())),
            };
            let payload = envelope.payload();
            match envelope.kind() {
                EventKind::GraphCreated => {
                    let Some(graph) = payload.get("graph").filter(|g| !g.is_null()) else {
                        return Ok(GraphReplay::Unsupported { first_seq: seq });
                    };
                    let graph: RuntimeGraph = serde_json::from_value(graph.clone())
                        .map_err(|e| GraphError::Ledger(format!("graph.created seq {seq}: {e}")))?;
                    graphs.insert(graph.graph_id, graph);
                }
                EventKind::GraphRevisionCommitted => {
                    let Some(proposal) = payload.get("proposal").filter(|p| !p.is_null()) else {
                        return Ok(GraphReplay::Unsupported { first_seq: seq });
                    };
                    let proposal: GraphProposal = serde_json::from_value(proposal.clone())
                        .map_err(|e| {
                            GraphError::Ledger(format!("graph.revision_committed seq {seq}: {e}"))
                        })?;
                    let graph_id = parse_graph_id(payload, seq)?;
                    let current = graphs.get(&graph_id).ok_or_else(|| {
                        GraphError::Ledger(format!("seq {seq}: revision for an unknown graph"))
                    })?;
                    let next = crate::proposal::validate_and_apply(current, &proposal)
                        .map_err(GraphError::Proposal)?;
                    graphs.insert(graph_id, next);
                }
                EventKind::GraphNodeStateChanged => {
                    let graph_id = parse_graph_id(payload, seq)?;
                    let node_id: NodeId = payload
                        .get("node_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| GraphError::Ledger(format!("seq {seq}: bad node_id")))?;
                    let state: NodeState = payload
                        .get("state")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .ok_or_else(|| GraphError::Ledger(format!("seq {seq}: bad state")))?;
                    let attempts = payload
                        .get("attempts")
                        .and_then(serde_json::Value::as_u64)
                        .map(|n| n as u32);
                    let graph = graphs.get_mut(&graph_id).ok_or_else(|| {
                        GraphError::Ledger(format!("seq {seq}: state change for an unknown graph"))
                    })?;
                    let node = graph.nodes.get_mut(&node_id).ok_or_else(|| {
                        GraphError::Ledger(format!("seq {seq}: state change for an unknown node"))
                    })?;
                    node.state = state;
                    if let Some(attempts) = attempts {
                        node.attempts = attempts;
                    }
                    node.wait_token = payload
                        .get("wait_token")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    graph.revision = graph.revision.saturating_add(1);
                }
                // The one acceptance record, reduced exactly as the live
                // path reduces it (GVS-006): the same goal and verification
                // nodes become `Succeeded`.
                EventKind::OrchestrationTaskAccepted => {
                    // A shared wire kind: the supervisor's own TaskAccepted
                    // maps onto the same string. Only a payload carrying
                    // this record's marker is ours; anything else belongs to
                    // another producer and must not poison the replay.
                    if payload.get("record").and_then(|v| v.as_str()) != Some(ACCEPTANCE_RECORD) {
                        continue;
                    }
                    let event: AcceptanceEvent = serde_json::from_value(payload.clone())
                        .map_err(|e| GraphError::Ledger(format!("acceptance seq {seq}: {e}")))?;
                    let Some(graph) = graphs.get_mut(&event.graph_id) else {
                        // Our record, but for a graph this session never
                        // created — another run's acceptance sharing the
                        // session. Not this replay's business.
                        continue;
                    };
                    for id in event.completed_nodes() {
                        let node = graph.nodes.get_mut(&id).ok_or_else(|| {
                            GraphError::Ledger(format!(
                                "seq {seq}: acceptance names an unknown node"
                            ))
                        })?;
                        if node.state == NodeState::Succeeded {
                            continue;
                        }
                        node.state = NodeState::Succeeded;
                        graph.revision = graph.revision.saturating_add(1);
                    }
                }
                // A rejected proposal changed nothing; every other kind
                // belongs to some other family.
                _ => {}
            }
        }
        Ok(GraphReplay::Rebuilt(graphs))
    }

    /// Append the single durable acceptance record. Nothing is reduced
    /// here: the caller applies [`Self::apply_accepted`] and the
    /// supervisor's own reduction only after this returns, so a failure to
    /// append leaves both projections untouched rather than half-advanced.
    pub fn append_acceptance(&mut self, event: &AcceptanceEvent) -> Result<(), GraphError> {
        let payload = serde_json::to_value(event)
            .map_err(|err| GraphError::Ledger(format!("acceptance payload: {err}")))?;
        self.append(EventKind::OrchestrationTaskAccepted, payload)
    }

    /// Reduce an already-durable acceptance into the in-memory graph: each
    /// named node becomes `Succeeded`. No append — the record this derives
    /// from is already on the ledger, and appending again would make
    /// acceptance several writes once more. Idempotent: a node already
    /// `Succeeded` is left alone, so replaying the same record cannot
    /// advance the revision twice, and a graph already fully reduced is
    /// left exactly as it was.
    ///
    /// Takes the nodes rather than the record so a caller holding only the
    /// graph's side of an acceptance — reconciling after an interrupted
    /// reduction — can call it without reconstructing the durable record.
    pub fn apply_accepted(
        &mut self,
        graph_id: GraphId,
        nodes: &[NodeId],
    ) -> Result<(), GraphError> {
        // Validate every node before changing any, so an acceptance naming
        // one unknown node cannot leave a half-reduced graph behind.
        {
            let graph = self.graphs.get(&graph_id).ok_or(GraphError::UnknownGraph)?;
            if nodes.iter().any(|id| graph.node(*id).is_none()) {
                return Err(GraphError::Proposal(ProposalError::UnknownNode));
            }
        }
        for id in nodes {
            let graph = self
                .graphs
                .get_mut(&graph_id)
                .ok_or(GraphError::UnknownGraph)?;
            let node = graph.nodes.get_mut(id).expect("checked above");
            if node.state == NodeState::Succeeded {
                continue;
            }
            node.state = NodeState::Succeeded;
            graph.revision = graph.revision.saturating_add(1);
            // One history entry per revision, as `set_state` records them,
            // so `diff` can still name every intermediate revision.
            let out = graph.clone();
            self.history.entry(graph_id).or_default().push(out);
        }
        Ok(())
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
