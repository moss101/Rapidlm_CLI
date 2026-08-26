//! PlaybookCompiler: parameterized playbook template -> initial RuntimeGraph.
//!
//! A playbook is a bounded, declarative step list with dependency edges. The
//! compiler validates the template (unique keys, resolvable acyclic
//! dependencies, single connected entry) and emits the initial graph at
//! revision 1. Compilation never executes anything.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use protocol::{GraphId, NodeId};

use crate::graph::{Edge, Node, RuntimeGraph};
use crate::kinds::{EdgeKind, NodeKind, NodeState};

/// Upper bound on steps per playbook.
pub const MAX_PLAYBOOK_STEPS: usize = 64;
/// Upper bound on declared dependencies per step.
pub const MAX_STEP_DEPENDENCIES: usize = 8;
/// Maximum UTF-8 bytes for a step key or playbook name.
pub const MAX_KEY_BYTES: usize = 128;
/// Maximum UTF-8 bytes for a human-readable label.
pub const MAX_LABEL_BYTES: usize = 256;

/// Typed playbook compilation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybookError {
    Empty,
    TooManySteps,
    InvalidKey,
    LabelTooLong,
    DuplicateStepKey,
    TooManyDependencies,
    UnknownDependency { key: String, missing: String },
    DependencyCycle,
    DisconnectedEntry,
}

impl core::fmt::Display for PlaybookError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Empty => "playbook has no steps",
            Self::TooManySteps => "playbook exceeds the step bound",
            Self::InvalidKey => "step key is empty, oversized, or malformed",
            Self::LabelTooLong => "step label exceeds the byte bound",
            Self::DuplicateStepKey => "playbook declares a duplicate step key",
            Self::TooManyDependencies => "step exceeds the dependency bound",
            Self::UnknownDependency { key, missing } => {
                let _ = key;
                "step depends on an undeclared step"
            }
            Self::DependencyCycle => "playbook dependencies form a cycle",
            Self::DisconnectedEntry => "a step is not reachable from the entry step",
        })
    }
}

impl std::error::Error for PlaybookError {}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
}

/// One playbook step: the node to create plus the steps it depends on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybookStep {
    key: String,
    kind: NodeKind,
    label: String,
    depends_on: Vec<String>,
    budget_tokens: u64,
    max_attempts: u32,
}

impl PlaybookStep {
    pub fn new(key: impl Into<String>, kind: NodeKind, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            kind,
            label: label.into(),
            depends_on: Vec::new(),
            budget_tokens: 0,
            max_attempts: 3,
        }
    }

    pub fn with_dependencies<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.depends_on = keys.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_budget(mut self, budget_tokens: u64) -> Self {
        self.budget_tokens = budget_tokens;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }
}

/// Parameterized playbook template. The first declared step is the entry.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct PlaybookTemplate {
    name: String,
    steps: Vec<PlaybookStep>,
}

impl PlaybookTemplate {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    pub fn named(&self) -> &str {
        &self.name
    }

    pub fn push(mut self, step: PlaybookStep) -> Self {
        self.steps.push(step);
        self
    }

    fn validate(&self) -> Result<(), PlaybookError> {
        if self.steps.is_empty() {
            return Err(PlaybookError::Empty);
        }
        if self.steps.len() > MAX_PLAYBOOK_STEPS {
            return Err(PlaybookError::TooManySteps);
        }
        let mut seen = BTreeSet::new();
        for step in &self.steps {
            if !valid_key(&step.key) {
                return Err(PlaybookError::InvalidKey);
            }
            if !seen.insert(step.key.as_str()) {
                return Err(PlaybookError::DuplicateStepKey);
            }
            if step.label.len() > MAX_LABEL_BYTES {
                return Err(PlaybookError::LabelTooLong);
            }
            if step.depends_on.len() > MAX_STEP_DEPENDENCIES {
                return Err(PlaybookError::TooManyDependencies);
            }
            for dep in &step.depends_on {
                if !self.steps.iter().any(|s| s.key == *dep) {
                    return Err(PlaybookError::UnknownDependency {
                        key: step.key.clone(),
                        missing: dep.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Topological order over dependency edges (Kahn). Detects cycles.
fn topo_order(steps: &[PlaybookStep]) -> Result<Vec<usize>, PlaybookError> {
    let index_of: BTreeMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str(), i))
        .collect();
    let mut indegree = vec![0usize; steps.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); steps.len()];
    for (i, step) in steps.iter().enumerate() {
        for dep in &step.depends_on {
            let j = index_of[dep.as_str()];
            indegree[i] += 1;
            dependents[j].push(i);
        }
    }
    let mut queue: VecDeque<usize> = (0..steps.len()).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(steps.len());
    while let Some(i) = queue.pop_front() {
        order.push(i);
        for &j in &dependents[i] {
            indegree[j] -= 1;
            if indegree[j] == 0 {
                queue.push_back(j);
            }
        }
    }
    if order.len() != steps.len() {
        return Err(PlaybookError::DependencyCycle);
    }
    Ok(order)
}

/// Compile a validated template into the initial [`RuntimeGraph`] at
/// revision 1. All nodes start `Pending`; dependency declarations become
/// executable `DependsOn` edges. Every step must be reachable from the
/// entry (first declared) step.
pub fn compile(
    template: &PlaybookTemplate,
    graph_id: GraphId,
) -> Result<RuntimeGraph, PlaybookError> {
    template.validate()?;
    let steps = &template.steps;
    let order = topo_order(steps)?;

    // Entry is the first declared step and must reach every other step.
    let entry_index = 0usize;
    let index_of: BTreeMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str(), i))
        .collect();
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::from([entry_index]);
    while let Some(i) = queue.pop_front() {
        if !reachable.insert(i) {
            continue;
        }
        for (j, step) in steps.iter().enumerate() {
            if step.depends_on.iter().any(|d| index_of[d.as_str()] == i) {
                queue.push_back(j);
            }
        }
    }
    if reachable.len() != steps.len() {
        return Err(PlaybookError::DisconnectedEntry);
    }

    // Deterministic node ids from traversal order keep export stable per run.
    let ids: Vec<NodeId> = steps.iter().map(|_| NodeId::new()).collect();
    let mut nodes = BTreeMap::new();
    let mut edges = Vec::new();
    let root = ids[entry_index];
    for (i, step) in steps.iter().enumerate() {
        let mut node = Node::new(ids[i], step.kind, step.label.clone());
        node.state = NodeState::Pending;
        node.budget_tokens = step.budget_tokens;
        node.max_attempts = step.max_attempts;
        nodes.insert(ids[i], node);
        for dep in &step.depends_on {
            let from = ids[index_of[dep.as_str()]];
            edges.push(Edge {
                from,
                to: ids[i],
                kind: EdgeKind::DependsOn,
                created_revision: 1,
                condition: Default::default(),
            });
        }
    }
    Ok(RuntimeGraph {
        graph_id,
        revision: 1,
        root,
        nodes,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal_graph_template() -> PlaybookTemplate {
        PlaybookTemplate::new("release-audit")
            .push(PlaybookStep::new("goal", NodeKind::Goal, "Ship v3"))
            .push(
                PlaybookStep::new("ctx", NodeKind::ContextQuery, "Load repo context")
                    .with_dependencies(["goal"]),
            )
            .push(
                PlaybookStep::new("plan", NodeKind::Plan, "Draft plan")
                    .with_dependencies(["ctx"])
                    .with_budget(4_000),
            )
            .push(
                PlaybookStep::new("verify", NodeKind::Claim, "Verify claims")
                    .with_dependencies(["plan"]),
            )
    }

    #[test]
    fn compiles_template_to_initial_pending_graph_with_depends_on_edges() {
        let template = goal_graph_template();
        let graph = compile(&template, GraphId::new()).expect("compile");
        assert_eq!(graph.revision, 1);
        assert_eq!(graph.nodes.len(), 4);
        assert_eq!(graph.edges.len(), 3);
        // Entry node is the root and every node starts Pending.
        let root_label = &graph.nodes.get(&graph.root).expect("root").label;
        assert_eq!(root_label, "Ship v3");
        for node in graph.nodes.values() {
            assert_eq!(node.state, NodeState::Pending);
        }
        // Dependency declarations become DependsOn edges into the dependent.
        let labels: BTreeMap<NodeId, &str> = graph
            .nodes
            .iter()
            .map(|(id, n)| (*id, n.label.as_str()))
            .collect();
        let by_to: BTreeMap<&str, Vec<&str>> = graph.edges.iter().fold(BTreeMap::new(), |mut m, e| {
            m.entry(labels[&e.to]).or_default().push(labels[&e.from]);
            m
        });
        assert_eq!(by_to["Draft plan"], vec!["Load repo context"]);
        assert_eq!(by_to["Verify claims"], vec!["Draft plan"]);
        assert_eq!(by_to["Load repo context"], vec!["Ship v3"]);
        // The entry step declares no dependencies so no edge targets it.
        assert!(!by_to.contains_key("Ship v3"));
        // Budget pin survives compilation.
        let plan = graph
            .nodes
            .values()
            .find(|n| n.label == "Draft plan")
            .expect("plan");
        assert_eq!(plan.budget_tokens, 4_000);
        // Initial ready set contains exactly the root.
        assert_eq!(graph.ready_set(), vec![graph.root]);
    }

    #[test]
    fn rejects_duplicate_unknown_and_oversized_declarations() {
        let dup = PlaybookTemplate::new("dup")
            .push(PlaybookStep::new("a", NodeKind::Task, "one"))
            .push(PlaybookStep::new("a", NodeKind::Task, "two"));
        assert_eq!(compile(&dup, GraphId::new()), Err(PlaybookError::DuplicateStepKey));

        let unknown = PlaybookTemplate::new("unknown")
            .push(PlaybookStep::new("a", NodeKind::Task, "one"))
            .push(
                PlaybookStep::new("b", NodeKind::Task, "two").with_dependencies(["ghost"]),
            );
        assert_eq!(
            compile(&unknown, GraphId::new()),
            Err(PlaybookError::UnknownDependency {
                key: "b".into(),
                missing: "ghost".into()
            })
        );

        let empty = PlaybookTemplate::new("empty");
        assert_eq!(compile(&empty, GraphId::new()), Err(PlaybookError::Empty));

        let long_label = "x".repeat(MAX_LABEL_BYTES + 1);
        let oversized =
            PlaybookTemplate::new("big").push(PlaybookStep::new("a", NodeKind::Task, long_label));
        assert_eq!(compile(&oversized, GraphId::new()), Err(PlaybookError::LabelTooLong));

        let bad_key = PlaybookTemplate::new("bad")
            .push(PlaybookStep::new("a b!", NodeKind::Task, "ok"));
        assert_eq!(compile(&bad_key, GraphId::new()), Err(PlaybookError::InvalidKey));
    }

    #[test]
    fn rejects_dependency_cycles_and_disconnected_steps() {
        let cycle = PlaybookTemplate::new("cycle")
            .push(PlaybookStep::new("a", NodeKind::Task, "a").with_dependencies(["b"]))
            .push(PlaybookStep::new("b", NodeKind::Task, "b").with_dependencies(["a"]));
        assert_eq!(compile(&cycle, GraphId::new()), Err(PlaybookError::DependencyCycle));

        let disconnected = PlaybookTemplate::new("split")
            .push(PlaybookStep::new("entry", NodeKind::Goal, "entry"))
            .push(PlaybookStep::new("island", NodeKind::Task, "unreachable"));
        assert_eq!(
            compile(&disconnected, GraphId::new()),
            Err(PlaybookError::DisconnectedEntry)
        );
    }

    #[test]
    fn diamond_template_compiles_and_ready_set_grows_after_completion() {
        let template = PlaybookTemplate::new("diamond")
            .push(PlaybookStep::new("start", NodeKind::Goal, "start"))
            .push(
                PlaybookStep::new("left", NodeKind::Task, "left").with_dependencies(["start"]),
            )
            .push(
                PlaybookStep::new("right", NodeKind::Task, "right").with_dependencies(["start"]),
            )
            .push(
                PlaybookStep::new("join", NodeKind::Artifact, "join")
                    .with_dependencies(["left", "right"]),
            );
        let mut graph = compile(&template, GraphId::new()).expect("compile");
        assert_eq!(graph.nodes.len(), 4);
        assert_eq!(graph.ready_set().len(), 1);
        // Complete the root: both mid-steps become ready, join stays blocked.
        let root = graph.root;
        graph.nodes.get_mut(&root).expect("root").state = NodeState::Succeeded;
        let ready = graph.ready_set();
        assert_eq!(ready.len(), 2);
    }
}
