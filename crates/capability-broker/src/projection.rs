//! Model-visible capability projection (FR-SEC-002, tool/capability subset).
//!
//! A projection narrows a *candidate* set (the user/org-policy-allowed set) by
//! the acting role, runtime node, mode and model tier. It can only remove
//! capabilities, never add one not present in `candidate`, so a projection can
//! never broaden policy. Execution still revalidates authority; projection only
//! reduces the model-visible attack/token surface.

use std::error::Error;
use std::fmt;

use super::capability::{Capability, CapabilityFamily};
use super::policy::evaluator::RiskClass;

/// All v1 capabilities, for callers that project the full surface.
pub const ALL_CAPABILITIES: &[Capability] = &[
    Capability::FsRead,
    Capability::FsWrite,
    Capability::ProcExec,
    Capability::NetConnect,
    Capability::GitWrite,
    Capability::SecretUse,
    Capability::BrowserNavigate,
    Capability::BrowserDownload,
    Capability::MobileControl,
    Capability::McpInvoke,
    Capability::PluginInvoke,
];

/// Role of the acting agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectionRole {
    Coder,
    Researcher,
    Operator,
    Auditor,
}

/// Runtime node the projection is computed for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectionNode {
    Context,
    Workspace,
    Shell,
    External,
    Browser,
}

/// Runtime mode. `Automation` (dont-ask) narrows interaction-heavy actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectionMode {
    Interactive,
    Automation,
}

/// Model capability tier. Small tiers only ever narrow, never add.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectionModel {
    Small,
    Balanced,
    Large,
}

/// Key for one projection: role / node / mode / model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProjectionScope {
    role: ProjectionRole,
    node: ProjectionNode,
    mode: ProjectionMode,
    model: ProjectionModel,
}

/// One projected capability with the family and coarse risk it carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProjectedCapability {
    capability: Capability,
    family: CapabilityFamily,
    risk: RiskClass,
}

/// The restricted, model-visible capability subset for a scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityProjection {
    scope: ProjectionScope,
    allowed: Vec<ProjectedCapability>,
}

impl ProjectionRole {
    pub const ALL: &'static [Self] =
        &[Self::Coder, Self::Researcher, Self::Operator, Self::Auditor];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coder => "coder",
            Self::Researcher => "researcher",
            Self::Operator => "operator",
            Self::Auditor => "auditor",
        }
    }
}

impl ProjectionNode {
    pub const ALL: &'static [Self] = &[
        Self::Context,
        Self::Workspace,
        Self::Shell,
        Self::External,
        Self::Browser,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Workspace => "workspace",
            Self::Shell => "shell",
            Self::External => "external",
            Self::Browser => "browser",
        }
    }
}

impl ProjectionMode {
    pub const ALL: &'static [Self] = &[Self::Interactive, Self::Automation];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Automation => "automation",
        }
    }
}

impl ProjectionModel {
    pub const ALL: &'static [Self] = &[Self::Small, Self::Balanced, Self::Large];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Balanced => "balanced",
            Self::Large => "large",
        }
    }
}

impl ProjectionScope {
    pub const fn new(
        role: ProjectionRole,
        node: ProjectionNode,
        mode: ProjectionMode,
        model: ProjectionModel,
    ) -> Self {
        Self {
            role,
            node,
            mode,
            model,
        }
    }

    pub const fn role(self) -> ProjectionRole {
        self.role
    }

    pub const fn node(self) -> ProjectionNode {
        self.node
    }

    pub const fn mode(self) -> ProjectionMode {
        self.mode
    }

    pub const fn model(self) -> ProjectionModel {
        self.model
    }
}

impl ProjectedCapability {
    fn new(capability: Capability) -> Self {
        Self {
            capability,
            family: family_of(capability),
            risk: risk_of(capability),
        }
    }

    pub const fn capability(self) -> Capability {
        self.capability
    }

    pub const fn family(self) -> CapabilityFamily {
        self.family
    }

    pub const fn risk(self) -> RiskClass {
        self.risk
    }
}

impl CapabilityProjection {
    /// Project `candidate` through the scope rules. `allowed` is always `⊆
    /// candidate`; a capability absent from `candidate` is never projected in.
    pub fn project(scope: ProjectionScope, candidate: &[Capability]) -> Self {
        let mut allowed = Vec::new();
        for &capability in candidate {
            if scope_allows(scope, capability)
                && !allowed
                    .iter()
                    .any(|item: &ProjectedCapability| item.capability() == capability)
            {
                allowed.push(ProjectedCapability::new(capability));
            }
        }
        Self { scope, allowed }
    }

    pub const fn scope(&self) -> ProjectionScope {
        self.scope
    }

    pub fn allowed(&self) -> &[ProjectedCapability] {
        &self.allowed
    }

    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.allowed
            .iter()
            .any(|item| item.capability() == capability)
    }
}

impl fmt::Display for CapabilityProjection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "projection(role={}, node={}, mode={}, model={}, {} caps)",
            self.scope.role.as_str(),
            self.scope.node.as_str(),
            self.scope.mode.as_str(),
            self.scope.model.as_str(),
            self.allowed.len(),
        )
    }
}

impl Error for CapabilityProjection {}

fn scope_allows(scope: ProjectionScope, capability: Capability) -> bool {
    role_allows(scope.role, capability)
        && node_allows(scope.node, capability)
        && !mode_denies(scope.mode, capability)
        && !model_denies(scope.model, capability)
}

fn role_allows(role: ProjectionRole, capability: Capability) -> bool {
    match role {
        ProjectionRole::Coder => matches!(
            capability,
            Capability::FsRead
                | Capability::FsWrite
                | Capability::ProcExec
                | Capability::GitWrite
                | Capability::PluginInvoke
                | Capability::McpInvoke
        ),
        ProjectionRole::Researcher => matches!(
            capability,
            Capability::FsRead
                | Capability::NetConnect
                | Capability::BrowserNavigate
                | Capability::BrowserDownload
                | Capability::McpInvoke
        ),
        ProjectionRole::Operator => matches!(
            capability,
            Capability::FsRead | Capability::FsWrite | Capability::ProcExec | Capability::GitWrite
        ),
        ProjectionRole::Auditor => matches!(
            capability,
            Capability::FsRead | Capability::SecretUse | Capability::NetConnect
        ),
    }
}

fn node_allows(node: ProjectionNode, capability: Capability) -> bool {
    match node {
        ProjectionNode::Context => matches!(
            capability,
            Capability::FsRead | Capability::NetConnect | Capability::BrowserNavigate
        ),
        ProjectionNode::Workspace => matches!(
            capability,
            Capability::FsRead
                | Capability::FsWrite
                | Capability::GitWrite
                | Capability::PluginInvoke
                | Capability::MobileControl
        ),
        ProjectionNode::Shell => {
            matches!(
                capability,
                Capability::FsRead | Capability::FsWrite | Capability::ProcExec
            )
        }
        ProjectionNode::External => matches!(
            capability,
            Capability::NetConnect | Capability::McpInvoke | Capability::PluginInvoke
        ),
        ProjectionNode::Browser => matches!(
            capability,
            Capability::FsRead | Capability::BrowserNavigate | Capability::BrowserDownload
        ),
    }
}

fn mode_denies(mode: ProjectionMode, capability: Capability) -> bool {
    // Automation (dont-ask) removes the interaction-heavy, confirm-prone actions.
    matches!(mode, ProjectionMode::Automation)
        && matches!(
            capability,
            Capability::MobileControl | Capability::PluginInvoke
        )
}

fn model_denies(model: ProjectionModel, capability: Capability) -> bool {
    // Small tiers only ever narrow: drop the most privileged/high-risk actions.
    matches!(model, ProjectionModel::Small)
        && matches!(
            capability,
            Capability::GitWrite | Capability::PluginInvoke | Capability::SecretUse
        )
}

fn family_of(capability: Capability) -> CapabilityFamily {
    match capability {
        Capability::FsRead | Capability::FsWrite => CapabilityFamily::Fs,
        Capability::ProcExec => CapabilityFamily::Proc,
        Capability::NetConnect => CapabilityFamily::Net,
        Capability::GitWrite => CapabilityFamily::Git,
        Capability::SecretUse => CapabilityFamily::Secret,
        Capability::BrowserNavigate | Capability::BrowserDownload => CapabilityFamily::Browser,
        Capability::MobileControl => CapabilityFamily::Mobile,
        Capability::McpInvoke => CapabilityFamily::Mcp,
        Capability::PluginInvoke => CapabilityFamily::Plugin,
    }
}

fn risk_of(capability: Capability) -> RiskClass {
    match capability {
        Capability::SecretUse
        | Capability::GitWrite
        | Capability::PluginInvoke
        | Capability::ProcExec => RiskClass::High,
        Capability::NetConnect | Capability::BrowserDownload | Capability::MobileControl => {
            RiskClass::Medium
        }
        _ => RiskClass::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(
        role: ProjectionRole,
        node: ProjectionNode,
        mode: ProjectionMode,
        model: ProjectionModel,
    ) -> ProjectionScope {
        ProjectionScope::new(role, node, mode, model)
    }

    #[test]
    fn projection_is_a_subset_of_candidate_and_never_broadens() {
        let candidate = ALL_CAPABILITIES.to_vec();
        let projected = CapabilityProjection::project(
            scope(
                ProjectionRole::Coder,
                ProjectionNode::Workspace,
                ProjectionMode::Interactive,
                ProjectionModel::Balanced,
            ),
            &candidate,
        );
        for item in projected.allowed() {
            assert!(
                candidate.contains(&item.capability()),
                "{} not in candidate",
                item.capability()
            );
        }
        // Adding a capability the role/node forbids must not appear.
        assert!(!projected.allows(Capability::SecretUse));
        assert!(!projected.allows(Capability::MobileControl));
    }

    #[test]
    fn privileged_tool_not_granted_is_excluded() {
        let candidate = ALL_CAPABILITIES.to_vec();
        let projected = CapabilityProjection::project(
            scope(
                ProjectionRole::Researcher,
                ProjectionNode::Context,
                ProjectionMode::Interactive,
                ProjectionModel::Balanced,
            ),
            &candidate,
        );
        assert!(projected.allows(Capability::FsRead));
        assert!(projected.allows(Capability::NetConnect));
        // Privileged/off-role capabilities are excluded.
        assert!(!projected.allows(Capability::ProcExec));
        assert!(!projected.allows(Capability::GitWrite));
        assert!(!projected.allows(Capability::PluginInvoke));
        assert!(!projected.allows(Capability::SecretUse));
    }

    #[test]
    fn automation_mode_and_small_model_narrow_further() {
        let candidate = ALL_CAPABILITIES.to_vec();
        let interactive = CapabilityProjection::project(
            scope(
                ProjectionRole::Coder,
                ProjectionNode::Workspace,
                ProjectionMode::Interactive,
                ProjectionModel::Balanced,
            ),
            &candidate,
        );
        let automation = CapabilityProjection::project(
            scope(
                ProjectionRole::Coder,
                ProjectionNode::Workspace,
                ProjectionMode::Automation,
                ProjectionModel::Balanced,
            ),
            &candidate,
        );
        assert!(interactive.allows(Capability::PluginInvoke));
        assert!(!automation.allows(Capability::PluginInvoke));
        assert!(automation.len() < interactive.len());

        let small = CapabilityProjection::project(
            scope(
                ProjectionRole::Coder,
                ProjectionNode::Workspace,
                ProjectionMode::Interactive,
                ProjectionModel::Small,
            ),
            &candidate,
        );
        assert!(!small.allows(Capability::GitWrite));
        assert!(!small.allows(Capability::PluginInvoke));
        assert!(small.allows(Capability::FsRead));
    }

    #[test]
    fn empty_candidate_projects_empty_and_display_reports_scope() {
        let projected = CapabilityProjection::project(
            scope(
                ProjectionRole::Auditor,
                ProjectionNode::Shell,
                ProjectionMode::Interactive,
                ProjectionModel::Balanced,
            ),
            &[],
        );
        assert!(projected.is_empty());
        assert_eq!(projected.len(), 0);
        let shown = projected.to_string();
        assert!(shown.contains("role=auditor"));
        assert!(shown.contains("node=shell"));
        assert!(!shown.contains("secret"));
    }

    #[test]
    fn projection_is_always_a_subset_of_candidate_invariant() {
        // Exhaustive over every role/node/mode/model scope: the projected set
        // is always ⊆ candidate and never contains a capability not present in
        // the candidate. Projection is visibility/narrowing only, never
        // authorization (there is no authorize/lease path on this type).
        for role in ProjectionRole::ALL {
            for node in ProjectionNode::ALL {
                for mode in ProjectionMode::ALL {
                    for model in ProjectionModel::ALL {
                        let scope = ProjectionScope::new(*role, *node, *mode, *model);
                        let projected = CapabilityProjection::project(scope, ALL_CAPABILITIES);
                        for item in projected.allowed() {
                            assert!(
                                ALL_CAPABILITIES.contains(&item.capability()),
                                "{:?} not in candidate",
                                item.capability()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn projected_capabilities_carry_family_and_risk() {
        let projected = CapabilityProjection::project(
            scope(
                ProjectionRole::Coder,
                ProjectionNode::Workspace,
                ProjectionMode::Interactive,
                ProjectionModel::Balanced,
            ),
            ALL_CAPABILITIES,
        );
        for item in projected.allowed() {
            assert_eq!(item.family(), family_of(item.capability()));
            assert_eq!(item.risk(), risk_of(item.capability()));
            assert!(!item.capability().to_string().is_empty());
        }
        let write = projected
            .allowed()
            .iter()
            .find(|item| item.capability() == Capability::FsWrite)
            .expect("fs.write projected");
        assert_eq!(write.family(), CapabilityFamily::Fs);
    }
}
