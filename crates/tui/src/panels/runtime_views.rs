//! Runtime inspector surfaces added by P10: graph, browser/computer,
//! resources/sandbox, ownership indicator, and timeline/checkpoint views.
//!
//! All are frontend projections over kernel state: bounded rows, sanitized
//! text, typed intents back to the kernel client. None of them mutate state
//! directly.

/// Maximum rows retained by any runtime view.
pub const MAX_VIEW_ROWS: usize = 256;
/// Maximum characters per rendered row; longer input is truncated.
pub const MAX_ROW_CHARS: usize = 120;

fn bounded(text: &str) -> String {
    if text.chars().count() > MAX_ROW_CHARS {
        text.chars().take(MAX_ROW_CHARS).collect()
    } else {
        text.to_owned()
    }
}

// ---------------------------------------------------------------------------
// P10-008 graph inspector

/// One node row of the graph inspector projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphNodeRow {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub label: String,
}

impl GraphNodeRow {
    pub fn new(id: &str, kind: &str, state: &str, label: &str) -> Option<Self> {
        if id.is_empty() || kind.is_empty() || state.is_empty() {
            return None;
        }
        Some(Self {
            id: bounded(id),
            kind: bounded(kind),
            state: bounded(state),
            label: bounded(label),
        })
    }
}

/// Pure graph inspector: sorted node rows + dependency edges as "from -> to".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphViewModel {
    nodes: Vec<GraphNodeRow>,
    edges: Vec<(String, String)>,
}

impl GraphViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a node row; rejects empty identifiers and enforces the bound.
    pub fn push_node(&mut self, row: GraphNodeRow) -> bool {
        if self.nodes.len() >= MAX_VIEW_ROWS
            || self.nodes.iter().any(|n| n.id == row.id)
        {
            return false;
        }
        self.nodes.push(row);
        self.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        true
    }

    pub fn push_edge(&mut self, from: &str, to: &str) -> bool {
        if self.edges.len() >= MAX_VIEW_ROWS || from.is_empty() || to.is_empty() {
            return false;
        }
        self.edges.push((bounded(from), bounded(to)));
        true
    }

    pub fn rows(&self) -> &[GraphNodeRow] {
        &self.nodes
    }

    /// Rendered edge lines, sorted for stable goldens.
    pub fn edge_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .edges
            .iter()
            .map(|(f, t)| format!("{f} -> {t}"))
            .collect();
        lines.sort();
        lines
    }
}

// ---------------------------------------------------------------------------
// P10-015 browser/computer panel

/// Outcome class surfaced by the computer panel for one action receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputerActionState {
    Pending,
    Completed,
    Failed,
    Stale,
}

/// Browser/computer panel: observation freshness + last actions, all bounded.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ComputerViewModel {
    observation_fresh: bool,
    takeover_generation: u64,
    actions: Vec<(String, ComputerActionState)>,
}

impl ComputerViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_observation(&mut self, fresh: bool, takeover_generation: u64) {
        self.observation_fresh = fresh;
        self.takeover_generation = takeover_generation;
    }

    pub fn push_action(&mut self, name: &str, state: ComputerActionState) -> bool {
        if name.is_empty() || self.actions.len() >= MAX_VIEW_ROWS {
            return false;
        }
        self.actions.push((bounded(name), state));
        true
    }

    pub fn observation_fresh(&self) -> bool {
        self.observation_fresh
    }

    pub fn takeover_generation(&self) -> u64 {
        self.takeover_generation
    }

    /// Human-readable status line; staleness must always be visible.
    pub fn status_line(&self) -> String {
        let freshness = if self.observation_fresh { "fresh" } else { "STALE" };
        format!(
            "observation={freshness} takeover_gen={} actions={}",
            self.takeover_generation,
            self.actions.len()
        )
    }
}

// ---------------------------------------------------------------------------
// P10-016 resources/sandbox panel

/// Sandbox posture for one resource class shown in the resources panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceRow {
    pub class: String,
    pub allowed: bool,
    pub scope: String,
}

impl ResourceRow {
    pub fn new(class: &str, allowed: bool, scope: &str) -> Option<Self> {
        if class.is_empty() {
            return None;
        }
        Some(Self {
            class: bounded(class),
            allowed,
            scope: bounded(scope),
        })
    }
}

/// Resources/sandbox panel projection with deterministic render order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourcesViewModel {
    rows: Vec<ResourceRow>,
}

impl ResourcesViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, row: ResourceRow) -> bool {
        if self.rows.len() >= MAX_VIEW_ROWS
            || self.rows.iter().any(|r| r.class == row.class)
        {
            return false;
        }
        self.rows.push(row);
        self.rows.sort_by(|a, b| a.class.cmp(&b.class));
        true
    }

    /// Rendered lines: `class allow|deny scope`.
    pub fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|r| {
                let verdict = if r.allowed { "allow" } else { "deny" };
                format!("{} {verdict} {}", r.class, r.scope)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// P10-021 human-control ownership indicator

/// Who owns the mutable control domain right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOwner {
    Agent,
    Human,
    Transitioning,
}

/// Persistent ownership indicator driven by takeover/control-lease events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipIndicator {
    owner: ControlOwner,
    generation: u64,
}

impl Default for OwnershipIndicator {
    fn default() -> Self {
        Self {
            owner: ControlOwner::Agent,
            generation: 0,
        }
    }
}

impl OwnershipIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a control event: "takeover", "return", or "generation:N".
    pub fn observe(&mut self, event: &str) -> Result<(), ()> {
        match event {
            "takeover" => {
                self.owner = ControlOwner::Human;
                Ok(())
            }
            "return" => {
                self.owner = ControlOwner::Transitioning;
                Ok(())
            }
            other => {
                let Some(gen_text) = other.strip_prefix("generation:") else {
                    return Err(());
                };
                self.generation = gen_text.parse::<u64>().map_err(|_| ())?;
                if self.owner == ControlOwner::Transitioning && self.generation > 0 {
                    self.owner = ControlOwner::Agent;
                }
                Ok(())
            }
        }
    }

    pub fn owner(&self) -> ControlOwner {
        self.owner
    }

    /// Status-bar text; HUMAN must be unmissable.
    pub fn badge(&self) -> &'static str {
        match self.owner {
            ControlOwner::Agent => "AGENT_CONTROL",
            ControlOwner::Human => "HUMAN_CONTROL",
            ControlOwner::Transitioning => "CONTROL_TRANSITION",
        }
    }
}

// ---------------------------------------------------------------------------
// P10-023 timeline/checkpoint view

/// One checkpoint marker on the session timeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointMarker {
    pub seq: u64,
    pub label: String,
}

/// Timeline/checkpoint view: ordered markers with rewind target selection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimelineViewModel {
    markers: Vec<CheckpointMarker>,
}

impl TimelineViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_checkpoint(&mut self, seq: u64, label: &str) -> bool {
        if label.is_empty() || self.markers.len() >= MAX_VIEW_ROWS {
            return false;
        }
        if self.markers.last().is_some_and(|m| m.seq >= seq) {
            return false;
        }
        self.markers.push(CheckpointMarker {
            seq,
            label: bounded(label),
        });
        true
    }

    pub fn markers(&self) -> &[CheckpointMarker] {
        &self.markers
    }

    /// Latest checkpoint at or before `seq` — the safe rewind target.
    pub fn rewind_target(&self, seq: u64) -> Option<&CheckpointMarker> {
        self.markers.iter().rev().find(|m| m.seq <= seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_view_sorts_nodes_renders_sorted_edges_and_bounds_rows() {
        let mut vm = GraphViewModel::new();
        assert!(vm.push_node(GraphNodeRow::new("b", "task", "pending", "B").unwrap()));
        assert!(vm.push_node(GraphNodeRow::new("a", "goal", "running", "A").unwrap()));
        assert!(!vm.push_node(GraphNodeRow::new("a", "goal", "x", "dup").unwrap()), "duplicate id rejected");
        assert!(vm.push_edge("b", "a"));
        assert_eq!(
            vm.rows().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "rows sort by id"
        );
        assert_eq!(vm.edge_lines(), vec!["b -> a"]);
        assert!(GraphNodeRow::new("", "task", "pending", "x").is_none());
    }

    #[test]
    fn computer_panel_surfaces_staleness_and_takeover_generation() {
        let mut vm = ComputerViewModel::new();
        vm.set_observation(true, 3);
        assert!(vm.push_action("click:submit", ComputerActionState::Completed));
        assert!(vm.push_action("type:text", ComputerActionState::Failed));
        assert_eq!(
            vm.status_line(),
            "observation=fresh takeover_gen=3 actions=2"
        );
        vm.set_observation(false, 7);
        assert!(!vm.observation_fresh());
        assert!(vm.status_line().contains("STALE"), "staleness must be visible");
        assert!(!vm.push_action("", ComputerActionState::Pending));
    }

    #[test]
    fn resources_panel_renders_deterministic_allow_deny_lines() {
        let mut vm = ResourcesViewModel::new();
        assert!(vm.push(ResourceRow::new("net.connect", false, "*").unwrap()));
        assert!(vm.push(ResourceRow::new("fs.read", true, "src/**/*.rs").unwrap()));
        // Duplicate class rejected regardless of verdict.
        assert!(!vm.push(ResourceRow::new("fs.read", false, "other").unwrap()));
        assert_eq!(
            vm.lines(),
            vec!["fs.read allow src/**/*.rs", "net.connect deny *"]
        );
    }

    #[test]
    fn ownership_indicator_tracks_takeover_return_and_generations() {
        let mut ind = OwnershipIndicator::new();
        assert_eq!(ind.badge(), "AGENT_CONTROL");
        ind.observe("takeover").expect("takeover");
        assert_eq!(ind.badge(), "HUMAN_CONTROL");
        // Generation bump alone during human control stays human.
        ind.observe("generation:5").expect("gen");
        assert_eq!(ind.badge(), "HUMAN_CONTROL");
        ind.observe("return").expect("return");
        assert_eq!(ind.badge(), "CONTROL_TRANSITION");
        ind.observe("generation:6").expect("gen2");
        assert_eq!(ind.badge(), "AGENT_CONTROL");
        assert!(ind.observe("bogus").is_err(), "unknown events rejected");
    }

    #[test]
    fn timeline_selects_safe_rewind_target_and_rejects_out_of_order() {
        let mut vm = TimelineViewModel::new();
        assert!(vm.push_checkpoint(10, "before patch"));
        assert!(vm.push_checkpoint(25, "green tests"));
        assert!(!vm.push_checkpoint(20, "out of order"), "seq must increase");
        assert_eq!(vm.rewind_target(30).expect("target").label, "green tests");
        assert_eq!(vm.rewind_target(25).expect("exact").seq, 25);
        assert_eq!(vm.rewind_target(5).map(|_| "some"), None, "no marker before first seq");
    }

    #[test]
    fn bounded_truncates_overlong_row_text() {
        let long = "x".repeat(MAX_ROW_CHARS * 3);
        let row = GraphNodeRow::new("id", "task", "pending", &long).unwrap();
        assert_eq!(row.label.chars().count(), MAX_ROW_CHARS);
    }
}
