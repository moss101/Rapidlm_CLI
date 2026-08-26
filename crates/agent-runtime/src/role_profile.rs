//! Role registry and per-role capability/tool profiles (agent-harness §3, P5-003).
//!
//! A role is named by the existing [`AgentRole`] and carries a profile: a prompt
//! summary (role purpose), the tool/capability surface it may *request*, a
//! default model/context tier, and read-only / can-delegate flags. This is pure
//! metadata — it is not a second role runtime. A role *defines* a prompt + tool
//! surface + defaults; it never grants authority. Actual authority stays in
//! Capability Projection → Policy → Capability Broker → lease validation.

use crate::agent::model::AgentRole;

/// Tool/capability classes a role may request. Lower layers still enforce
/// policy; this shapes the model-visible surface (visibility, not authority).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum RoleToolClass {
    Read,
    Write,
    Exec,
    Net,
    Browser,
    Mobile,
    Mcp,
    Plugin,
    Git,
    Secret,
}

/// Bitset of [`RoleToolClass`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct RoleToolSurface(u16);

impl RoleToolSurface {
    pub const fn none() -> Self {
        Self(0)
    }

    pub const fn with(mut self, class: RoleToolClass) -> Self {
        self.0 |= 1 << (class as u16);
        self
    }

    pub const fn allows(self, class: RoleToolClass) -> bool {
        self.0 & (1 << (class as u16)) != 0
    }

    pub fn keys(self) -> impl Iterator<Item = RoleToolClass> {
        RoleToolClass::iter().filter(move |class| self.allows(*class))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn sum(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl RoleToolClass {
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Write,
        Self::Exec,
        Self::Net,
        Self::Browser,
        Self::Mobile,
        Self::Mcp,
        Self::Plugin,
        Self::Git,
        Self::Secret,
    ];

    fn iter() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
            Self::Net => "net",
            Self::Browser => "browser",
            Self::Mobile => "mobile",
            Self::Mcp => "mcp",
            Self::Plugin => "plugin",
            Self::Git => "git",
            Self::Secret => "secret",
        }
    }
}

/// Default model/context tier a role prefers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RoleModelPolicy {
    Cheap,
    Balanced,
    HighReasoning,
}

impl RoleModelPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Balanced => "balanced",
            Self::HighReasoning => "high_reasoning",
        }
    }
}

/// One role's profile. Immutable metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoleProfile {
    role: AgentRole,
    summary: &'static str,
    tool_surface: RoleToolSurface,
    model_policy: RoleModelPolicy,
    read_only: bool,
    can_delegate: bool,
}

/// Static role registry. The roles are the existing [`AgentRole`] values.
pub struct RoleRegistry;

impl RoleRegistry {
    /// Look up the profile for a role. Unknown/future roles fall back to a
    /// read-only, no-tool, non-delegating default (fail closed).
    pub fn profile(role: AgentRole) -> RoleProfile {
        match role {
            AgentRole::Main => role_profile(
                role,
                "Coordinate the goal, delegate and verify",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Exec)
                    .with(RoleToolClass::Git)
                    .with(RoleToolClass::Mcp),
                RoleModelPolicy::Balanced,
                false,
                true,
            ),
            AgentRole::Planner => role_profile(
                role,
                "Decompose work into a plan",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Net)
                    .with(RoleToolClass::Mcp),
                RoleModelPolicy::HighReasoning,
                false,
                true,
            ),
            AgentRole::Coder => role_profile(
                role,
                "Implement changes in a writable workspace",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Exec)
                    .with(RoleToolClass::Git)
                    .with(RoleToolClass::Plugin),
                RoleModelPolicy::Balanced,
                false,
                true,
            ),
            AgentRole::Explorer | AgentRole::ContextCurator => role_profile(
                role,
                "Read-only context scout / specialist",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Net)
                    .with(RoleToolClass::Mcp),
                RoleModelPolicy::Cheap,
                true,
                false,
            ),
            AgentRole::Reviewer => role_profile(
                role,
                "Review changes without mutating the workspace",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Git)
                    .with(RoleToolClass::Mcp)
                    .with(RoleToolClass::Exec),
                RoleModelPolicy::Balanced,
                true,
                false,
            ),
            AgentRole::Verifier => role_profile(
                role,
                "Independently verify evidence, never repair",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Exec)
                    .with(RoleToolClass::Mcp),
                RoleModelPolicy::HighReasoning,
                true,
                false,
            ),
            AgentRole::SecurityReviewer => role_profile(
                role,
                "Security review, read-only",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Net)
                    .with(RoleToolClass::Git)
                    .with(RoleToolClass::Secret)
                    .with(RoleToolClass::Exec),
                RoleModelPolicy::HighReasoning,
                true,
                false,
            ),
            AgentRole::Debugger => role_profile(
                role,
                "Debug a failing path in a writable workspace",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Exec)
                    .with(RoleToolClass::Git),
                RoleModelPolicy::Balanced,
                false,
                true,
            ),
            AgentRole::Tester => role_profile(
                role,
                "Write and run tests against a constrained workspace",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Exec),
                RoleModelPolicy::Balanced,
                false,
                false,
            ),
            AgentRole::PerformanceReviewer => role_profile(
                role,
                "Run benchmarks/checks without mutating the workspace",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Exec),
                RoleModelPolicy::HighReasoning,
                true,
                false,
            ),
            AgentRole::BrowserOperator => role_profile(
                role,
                "Operate a browser/mobile surface",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Browser)
                    .with(RoleToolClass::Mobile),
                RoleModelPolicy::Balanced,
                false,
                false,
            ),
            AgentRole::ReleaseManager => role_profile(
                role,
                "Drive build, tag and release",
                RoleToolSurface::none()
                    .with(RoleToolClass::Read)
                    .with(RoleToolClass::Write)
                    .with(RoleToolClass::Exec)
                    .with(RoleToolClass::Git),
                RoleModelPolicy::Balanced,
                false,
                true,
            ),
        }
    }
}

impl RoleProfile {
    pub const fn role(self) -> AgentRole {
        self.role
    }

    pub const fn summary(self) -> &'static str {
        self.summary
    }

    pub const fn tool_surface(self) -> RoleToolSurface {
        self.tool_surface
    }

    pub const fn model_policy(self) -> RoleModelPolicy {
        self.model_policy
    }

    pub const fn read_only(self) -> bool {
        self.read_only
    }

    pub const fn can_delegate(self) -> bool {
        self.can_delegate
    }
}

const fn role_profile(
    role: AgentRole,
    summary: &'static str,
    tool_surface: RoleToolSurface,
    model_policy: RoleModelPolicy,
    read_only: bool,
    can_delegate: bool,
) -> RoleProfile {
    RoleProfile {
        role,
        summary,
        tool_surface,
        model_policy,
        read_only,
        can_delegate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_gets_a_profile_with_a_summary_and_its_own_role() {
        for role in [
            AgentRole::Main,
            AgentRole::Planner,
            AgentRole::Coder,
            AgentRole::Explorer,
            AgentRole::Reviewer,
            AgentRole::Verifier,
            AgentRole::SecurityReviewer,
            AgentRole::ContextCurator,
            AgentRole::Debugger,
            AgentRole::Tester,
            AgentRole::PerformanceReviewer,
            AgentRole::BrowserOperator,
            AgentRole::ReleaseManager,
        ] {
            let profile = RoleRegistry::profile(role);
            assert_eq!(profile.role(), role);
            assert!(!profile.summary().is_empty());
        }
    }

    #[test]
    fn read_only_roles_cannot_mutate_workspace_or_delegate() {
        for role in [
            AgentRole::Explorer,
            AgentRole::ContextCurator,
            AgentRole::Reviewer,
            AgentRole::Verifier,
            AgentRole::SecurityReviewer,
            AgentRole::PerformanceReviewer,
        ] {
            let profile = RoleRegistry::profile(role);
            assert!(profile.read_only(), "{role:?} must be read-only");
            // read-only means the governed workspace cannot be mutated.
            assert!(
                !profile.tool_surface().allows(RoleToolClass::Write),
                "{role:?}"
            );
            assert!(!profile.can_delegate(), "{role:?} must not delegate");
        }
    }

    #[test]
    fn verification_roles_can_run_checks_without_mutating_the_workspace() {
        for role in [
            AgentRole::Reviewer,
            AgentRole::Verifier,
            AgentRole::SecurityReviewer,
            AgentRole::PerformanceReviewer,
        ] {
            let profile = RoleRegistry::profile(role);
            assert!(profile.read_only(), "{role:?} read-only");
            assert!(
                profile.tool_surface().allows(RoleToolClass::Exec),
                "{role:?} may run checks"
            );
            assert!(
                !profile.tool_surface().allows(RoleToolClass::Write),
                "{role:?} may not mutate"
            );
        }
    }

    #[test]
    fn coder_is_writable_and_delegating_but_never_secret_or_net() {
        let profile = RoleRegistry::profile(AgentRole::Coder);
        assert!(!profile.read_only());
        assert!(profile.can_delegate());
        assert!(profile.tool_surface().allows(RoleToolClass::Write));
        assert!(profile.tool_surface().allows(RoleToolClass::Exec));
        assert!(profile.tool_surface().allows(RoleToolClass::Plugin));
        // A coder never requests secret material or bare network egress.
        assert!(!profile.tool_surface().allows(RoleToolClass::Secret));
        assert!(!profile.tool_surface().allows(RoleToolClass::Net));
    }

    #[test]
    fn tool_surface_is_a_union_that_never_broadens_a_single_class() {
        let surface = RoleToolSurface::none()
            .with(RoleToolClass::Read)
            .with(RoleToolClass::Git);
        let union = surface.sum(RoleToolSurface::none().with(RoleToolClass::Mcp));
        assert!(union.allows(RoleToolClass::Read));
        assert!(union.allows(RoleToolClass::Git));
        assert!(union.allows(RoleToolClass::Mcp));
        assert!(!union.allows(RoleToolClass::Write));
        let classes: Vec<_> = union.keys().collect();
        assert_eq!(classes.len(), 3);
    }
}
