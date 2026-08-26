//! Host-validated graph proposals. Models cannot apply these themselves.

use std::collections::{BTreeMap, BTreeSet};

use protocol::NodeId;
use serde::{Deserialize, Serialize};

use crate::graph::{Edge, EdgeCondition, Node, RuntimeGraph};
use crate::kinds::{EdgeKind, NodeKind, NodeState};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: NodeId,
    pub kind: NodeKind,
    pub label: String,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub resource_key: Option<String>,
    #[serde(default)]
    pub budget_tokens: u64,
}

impl NodeSpec {
    pub fn new(id: NodeId, kind: NodeKind, label: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            label: label.into(),
            workspace_key: None,
            resource_key: None,
            budget_tokens: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    #[serde(default)]
    pub condition: EdgeCondition,
}

/// Planner/model proposal. Host validates then appends a revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphProposal {
    pub base_revision: u64,
    pub add_nodes: Vec<NodeSpec>,
    pub add_edges: Vec<EdgeSpec>,
    #[serde(default)]
    pub supersede: Vec<NodeId>,
    #[serde(default)]
    pub invalidate: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalError {
    StaleRevision { expected: u64, found: u64 },
    DuplicateNode,
    UnknownNode,
    Cycle,
    EmptyLabel,
}

pub fn validate_and_apply(
    graph: &RuntimeGraph,
    proposal: &GraphProposal,
) -> Result<RuntimeGraph, ProposalError> {
    if proposal.base_revision != graph.revision {
        return Err(ProposalError::StaleRevision {
            expected: graph.revision,
            found: proposal.base_revision,
        });
    }
    let mut next = graph.clone();
    next.revision = graph.revision.saturating_add(1);

    for spec in &proposal.add_nodes {
        if spec.label.is_empty() {
            return Err(ProposalError::EmptyLabel);
        }
        if next.nodes.contains_key(&spec.id) {
            return Err(ProposalError::DuplicateNode);
        }
        let mut node = Node::new(spec.id, spec.kind, spec.label.clone());
        node.workspace_key = spec.workspace_key.clone();
        node.resource_key = spec.resource_key.clone();
        node.budget_tokens = spec.budget_tokens;
        next.nodes.insert(spec.id, node);
    }
    for spec in &proposal.add_edges {
        if !next.nodes.contains_key(&spec.from) || !next.nodes.contains_key(&spec.to) {
            return Err(ProposalError::UnknownNode);
        }
        next.edges.push(Edge {
            from: spec.from,
            to: spec.to,
            kind: spec.kind,
            created_revision: next.revision,
            condition: spec.condition,
        });
    }
    for id in &proposal.supersede {
        let node = next.nodes.get_mut(id).ok_or(ProposalError::UnknownNode)?;
        node.state = NodeState::Superseded;
    }
    for id in &proposal.invalidate {
        let node = next.nodes.get_mut(id).ok_or(ProposalError::UnknownNode)?;
        node.state = NodeState::Invalidated;
    }
    if has_executable_cycle(&next) {
        return Err(ProposalError::Cycle);
    }
    Ok(next)
}

fn has_executable_cycle(graph: &RuntimeGraph) -> bool {
    let mut adj: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    for e in &graph.edges {
        if e.kind.is_executable_dependency() {
            adj.entry(e.from).or_default().push(e.to);
        }
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    fn dfs(
        n: NodeId,
        adj: &BTreeMap<NodeId, Vec<NodeId>>,
        visiting: &mut BTreeSet<NodeId>,
        visited: &mut BTreeSet<NodeId>,
    ) -> bool {
        if visited.contains(&n) {
            return false;
        }
        if !visiting.insert(n) {
            return true;
        }
        if let Some(succ) = adj.get(&n) {
            for s in succ {
                if dfs(*s, adj, visiting, visited) {
                    return true;
                }
            }
        }
        visiting.remove(&n);
        visited.insert(n);
        false
    }
    graph
        .nodes
        .keys()
        .any(|id| dfs(*id, &adj, &mut visiting, &mut visited))
}
