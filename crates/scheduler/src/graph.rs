//! Immutable-revision Runtime Graph IR.

use std::collections::{BTreeMap, BTreeSet};

use protocol::{GraphId, NodeId};
use serde::{Deserialize, Serialize};

use crate::kinds::{EdgeKind, NodeKind, NodeState};

/// When a dependency edge is satisfied.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    #[default]
    PredecessorSucceeded,
    PredecessorFailed,
    Always,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub state: NodeState,
    pub label: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub resource_key: Option<String>,
    #[serde(default)]
    pub wait_token: Option<String>,
    #[serde(default)]
    pub budget_tokens: u64,
    #[serde(default)]
    pub spent_tokens: u64,
}

fn default_max_attempts() -> u32 {
    3
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub created_revision: u64,
    #[serde(default)]
    pub condition: EdgeCondition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeGraph {
    pub graph_id: GraphId,
    pub revision: u64,
    pub root: NodeId,
    pub nodes: BTreeMap<NodeId, Node>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadyContext {
    pub busy_resources: BTreeSet<String>,
    pub busy_workspaces: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphDiff {
    pub from_revision: u64,
    pub to_revision: u64,
    pub nodes_added: Vec<NodeId>,
    pub nodes_removed: Vec<NodeId>,
    pub state_changed: Vec<NodeId>,
    pub edges_added: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeExplain {
    pub node_id: NodeId,
    pub state: NodeState,
    pub ready: bool,
    pub reasons: Vec<String>,
}

impl Node {
    pub fn new(id: NodeId, kind: NodeKind, label: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            state: NodeState::Pending,
            label: label.into(),
            attempts: 0,
            max_attempts: 3,
            workspace_key: None,
            resource_key: None,
            wait_token: None,
            budget_tokens: 0,
            spent_tokens: 0,
        }
    }
}

impl RuntimeGraph {
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn ready_set(&self) -> Vec<NodeId> {
        self.ready_set_with(&ReadyContext::default())
    }

    pub fn ready_set_with(&self, ctx: &ReadyContext) -> Vec<NodeId> {
        let mut ready: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| n.state == NodeState::Pending)
            .filter(|n| self.is_ready(n.id, ctx).0)
            .map(|n| n.id)
            .collect();
        ready.sort_by_key(|id| id.to_string());
        ready
    }

    pub fn explain_ready(&self, id: NodeId) -> Option<NodeExplain> {
        let node = self.nodes.get(&id)?;
        let (ready, reasons) = self.is_ready(id, &ReadyContext::default());
        Some(NodeExplain {
            node_id: id,
            state: node.state,
            ready,
            reasons,
        })
    }

    pub fn explain_blocked(&self, id: NodeId) -> Option<NodeExplain> {
        self.explain_ready(id)
    }

    pub fn diff(&self, older: &RuntimeGraph) -> GraphDiff {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();
        for id in self.nodes.keys() {
            if !older.nodes.contains_key(id) {
                added.push(*id);
            } else if older.nodes[id].state != self.nodes[id].state {
                changed.push(*id);
            }
        }
        for id in older.nodes.keys() {
            if !self.nodes.contains_key(id) {
                removed.push(*id);
            }
        }
        added.sort_by_key(|id| id.to_string());
        removed.sort_by_key(|id| id.to_string());
        changed.sort_by_key(|id| id.to_string());
        GraphDiff {
            from_revision: older.revision,
            to_revision: self.revision,
            nodes_added: added,
            nodes_removed: removed,
            state_changed: changed,
            edges_added: self.edges.len().saturating_sub(older.edges.len()),
        }
    }

    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn export_dot(&self) -> String {
        let mut out = String::from("digraph G {\n");
        for n in self.nodes.values() {
            out.push_str(&format!(
                "  \"{}\" [label=\"{}:{}\"];\n",
                n.id,
                n.kind.as_str(),
                n.state.as_str()
            ));
        }
        for e in &self.edges {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
                e.from,
                e.to,
                e.kind.as_str()
            ));
        }
        out.push_str("}\n");
        out
    }

    pub fn export_mermaid(&self) -> String {
        let mut out = String::from("flowchart TD\n");
        for e in &self.edges {
            out.push_str(&format!(
                "  {}[{}] -->|{}| {}[{}]\n",
                e.from,
                self.nodes
                    .get(&e.from)
                    .map(|n| n.kind.as_str())
                    .unwrap_or("?"),
                e.kind.as_str(),
                e.to,
                self.nodes
                    .get(&e.to)
                    .map(|n| n.kind.as_str())
                    .unwrap_or("?")
            ));
        }
        out
    }

    pub fn is_ready(&self, id: NodeId, ctx: &ReadyContext) -> (bool, Vec<String>) {
        let Some(node) = self.nodes.get(&id) else {
            return (false, vec!["unknown node".into()]);
        };
        let mut reasons = Vec::new();
        if node.state != NodeState::Pending {
            reasons.push(format!("state is {}", node.state.as_str()));
            return (false, reasons);
        }
        if node.budget_tokens > 0 && node.spent_tokens >= node.budget_tokens {
            reasons.push("budget exhausted".into());
            return (false, reasons);
        }
        if let Some(key) = &node.resource_key
            && ctx.busy_resources.contains(key)
        {
            reasons.push(format!("resource {key} busy"));
        }
        if let Some(key) = &node.workspace_key
            && ctx.busy_workspaces.contains(key)
        {
            reasons.push(format!("workspace {key} write conflict"));
        }
        if matches!(node.kind, NodeKind::Join | NodeKind::Barrier) {
            let preds: Vec<_> = self
                .edges
                .iter()
                .filter(|e| e.to == id && matches!(e.kind, EdgeKind::JoinsAt | EdgeKind::DependsOn))
                .collect();
            if preds.is_empty() {
                reasons.push("join/barrier has no predecessors".into());
            }
            for e in &preds {
                let ok = self
                    .nodes
                    .get(&e.from)
                    .is_some_and(|n| n.state == NodeState::Succeeded);
                if !ok {
                    reasons.push(format!("join predecessor {} not succeeded", e.from));
                }
            }
        }
        for e in self
            .edges
            .iter()
            .filter(|e| e.to == id && e.kind.is_executable_dependency())
        {
            let pred = self.nodes.get(&e.from);
            let ok = match e.condition {
                EdgeCondition::Always => true,
                EdgeCondition::PredecessorSucceeded => {
                    pred.is_some_and(|n| n.state == NodeState::Succeeded)
                }
                EdgeCondition::PredecessorFailed => {
                    pred.is_some_and(|n| n.state == NodeState::Failed)
                }
            };
            if !ok {
                reasons.push(format!(
                    "dependency {} ({}) unsatisfied",
                    e.from,
                    e.condition_label()
                ));
            }
        }
        let ready = reasons.is_empty();
        if ready {
            reasons.push("all dependencies and constraints satisfied".into());
        }
        (ready, reasons)
    }
}

impl Edge {
    fn condition_label(&self) -> &'static str {
        match self.condition {
            EdgeCondition::PredecessorSucceeded => "succeeded",
            EdgeCondition::PredecessorFailed => "failed",
            EdgeCondition::Always => "always",
        }
    }
}
