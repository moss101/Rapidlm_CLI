//! Message phase routing: which configured profile serves each request purpose.
//!
//! The agent loop issues model calls from several phases — the main
//! conversation, context compaction, transcript titling, eval assertions.
//! Routing every phase to the primary model overspends: auxiliary phases can
//! name a cheaper profile while defaulting to the conversation model when no
//! override is configured. This module owns the purpose-name mapping used by
//! configuration, the reasoning-effort ladder, and the phase → profile
//! resolution. It is pure configuration: credential resolution and transport
//! stay in the adapter layers.
//!
//! Effort reaches the wire today on the OpenAI-compatible chat-completions
//! payload. The Anthropic adapter does not emit thinking budgets yet; until
//! it does, a configured effort there is validated and carried but inert.

use crate::credentials::ProfileId;
use crate::provider::ModelPurpose;

/// Number of [`ModelPurpose`] variants a [`PhaseRoute`] can override.
///
/// `ModelPurpose` is `#[non_exhaustive]`, so unknown future variants fold
/// into the last slot instead of panicking.
pub const MAX_PURPOSE_SLOTS: usize = 7;

/// Versioned phase-route shape marker for diagnostics surfaces.
pub const PHASE_ROUTE_SCHEMA: &str = "rapidlm.llm.phase_route.v1";

/// Reasoning-effort ladder accepted in configuration.
///
/// Ordering is by increasing effort: [`ReasoningEffort::None`] spends the
/// least, [`ReasoningEffort::Ultra`] the most.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Ultra,
}

/// Canonical lowercase names, in ladder order.
pub const REASONING_EFFORT_NAMES: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "ultra"];

/// Longest accepted effort name; longer input is reported as truncated.
const MAX_EFFORT_NAME_BYTES: usize = 8;

/// Typed parse failure for a configured effort name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReasoningEffortParseError;

impl ReasoningEffortParseError {
    /// Bounded echo of the rejected value for diagnostics; never echoes more
    /// than [`MAX_EFFORT_NAME_BYTES`] bytes.
    pub fn describe_input(&self, raw: &str) -> String {
        if raw.len() <= MAX_EFFORT_NAME_BYTES {
            raw.to_owned()
        } else {
            format!("{}…", &raw[..MAX_EFFORT_NAME_BYTES])
        }
    }
}

impl std::fmt::Display for ReasoningEffortParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid reasoning_effort: expected none|minimal|low|medium|high|xhigh|ultra")
    }
}

impl std::error::Error for ReasoningEffortParseError {}

impl ReasoningEffort {
    /// Parse a configuration value. Case-insensitive ASCII.
    pub fn parse(raw: &str) -> Result<Self, ReasoningEffortParseError> {
        let lowered = raw.to_ascii_lowercase();
        match lowered.as_str() {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "ultra" => Ok(Self::Ultra),
            _ => Err(ReasoningEffortParseError),
        }
    }

    /// Canonical lowercase name used on the wire.
    pub fn name(self) -> &'static str {
        REASONING_EFFORT_NAMES[self as usize]
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Configuration-side name of a [`ModelPurpose`]: snake_case variant name.
pub fn purpose_name(purpose: ModelPurpose) -> &'static str {
    match purpose {
        ModelPurpose::Chat => "chat",
        ModelPurpose::Code => "code",
        ModelPurpose::Plan => "plan",
        ModelPurpose::Review => "review",
        ModelPurpose::Compact => "compact",
        ModelPurpose::Embed => "embed",
        ModelPurpose::ComputerUse => "computer_use",
    }
}

/// Parse a configuration-side [`ModelPurpose`] name (inverse of
/// [`purpose_name`]). Case-insensitive ASCII.
pub fn parse_purpose_name(raw: &str) -> Option<ModelPurpose> {
    match raw.to_ascii_lowercase().as_str() {
        "chat" => Some(ModelPurpose::Chat),
        "code" => Some(ModelPurpose::Code),
        "plan" => Some(ModelPurpose::Plan),
        "review" => Some(ModelPurpose::Review),
        "compact" => Some(ModelPurpose::Compact),
        "embed" => Some(ModelPurpose::Embed),
        "computer_use" => Some(ModelPurpose::ComputerUse),
        _ => None,
    }
}

/// Per-purpose profile assignments with a mandatory conversation default.
///
/// Phases without an override resolve to `main`, so a missing auxiliary
/// mapping is fail-open by design: a cheap title model is an optimization,
/// not a dependency. Constructing overrides from raw strings is the config
/// layer's job; every [`ProfileId`] here is already validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhaseRoute {
    main: ProfileId,
    overrides: [Option<ProfileId>; MAX_PURPOSE_SLOTS],
}

impl PhaseRoute {
    /// Route every purpose to `main` until an override is set.
    pub fn new(main: ProfileId) -> Self {
        Self {
            main,
            overrides: [const { None }; MAX_PURPOSE_SLOTS],
        }
    }

    /// Builder-style override for one purpose.
    pub fn with_override(mut self, purpose: ModelPurpose, profile: ProfileId) -> Self {
        self.overrides[slot(purpose)] = Some(profile);
        self
    }

    /// The profile serving `purpose`: its override, else the main profile.
    pub fn route(&self, purpose: ModelPurpose) -> &ProfileId {
        self.overrides[slot(purpose)].as_ref().unwrap_or(&self.main)
    }

    /// The explicit override for `purpose`, if one is configured.
    pub fn override_for(&self, purpose: ModelPurpose) -> Option<&ProfileId> {
        self.overrides[slot(purpose)].as_ref()
    }

    /// The conversation default all phases fall back to.
    pub fn main(&self) -> &ProfileId {
        &self.main
    }
}

fn slot(purpose: ModelPurpose) -> usize {
    match purpose {
        ModelPurpose::Chat => 0,
        ModelPurpose::Code => 1,
        ModelPurpose::Plan => 2,
        ModelPurpose::Review => 3,
        ModelPurpose::Compact => 4,
        ModelPurpose::Embed => 5,
        _ => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(raw: &str) -> ProfileId {
        ProfileId::parse(raw).expect("valid profile id")
    }

    #[test]
    fn effort_ladder_parses_canonical_and_mixed_case() {
        for (raw, expected) in [
            ("none", ReasoningEffort::None),
            ("MINIMAL", ReasoningEffort::Minimal),
            ("Low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::XHigh),
            ("ULTRA", ReasoningEffort::Ultra),
        ] {
            assert_eq!(ReasoningEffort::parse(raw), Ok(expected), "{raw}");
            assert_eq!(expected.name(), raw.to_ascii_lowercase());
        }
    }

    #[test]
    fn effort_parse_rejects_unknown_and_bounds_the_echo() {
        let err = ReasoningEffort::parse("maximum").expect_err("unknown effort");
        assert_eq!(err.describe_input("maximum"), "maximum");
        assert_eq!(
            err.describe_input("an-extremely-long-effort-name"),
            "an-extre…".to_owned()
        );
        assert_eq!(ReasoningEffort::parse(""), Err(ReasoningEffortParseError));
    }

    #[test]
    fn effort_orders_by_spend() {
        assert!(ReasoningEffort::None < ReasoningEffort::Minimal);
        assert!(ReasoningEffort::Medium < ReasoningEffort::High);
        assert!(ReasoningEffort::XHigh < ReasoningEffort::Ultra);
    }

    #[test]
    fn purpose_names_round_trip() {
        for name in [
            "chat",
            "code",
            "plan",
            "review",
            "compact",
            "embed",
            "computer_use",
        ] {
            let purpose = parse_purpose_name(name)
                .unwrap_or_else(|| panic!("purpose name {name} must parse"));
            assert_eq!(purpose_name(purpose), name);
        }
        assert_eq!(parse_purpose_name("COMPACT"), Some(ModelPurpose::Compact));
        assert_eq!(parse_purpose_name("video"), None);
    }

    #[test]
    fn unconfigured_purposes_fall_back_to_main() {
        let route = PhaseRoute::new(profile("work"));
        assert_eq!(route.main(), &profile("work"));
        assert_eq!(route.route(ModelPurpose::Chat), &profile("work"));
        assert_eq!(route.route(ModelPurpose::Compact), &profile("work"));
        assert_eq!(route.override_for(ModelPurpose::Compact), None);
    }

    #[test]
    fn overrides_apply_only_to_their_purpose() {
        let route = PhaseRoute::new(profile("work"))
            .with_override(ModelPurpose::Compact, profile("cheap"))
            .with_override(ModelPurpose::Review, profile("strong"));
        assert_eq!(route.route(ModelPurpose::Compact), &profile("cheap"));
        assert_eq!(route.route(ModelPurpose::Review), &profile("strong"));
        assert_eq!(route.route(ModelPurpose::Chat), &profile("work"));
        assert_eq!(
            route.override_for(ModelPurpose::Compact),
            Some(&profile("cheap"))
        );
        assert_eq!(route.override_for(ModelPurpose::Chat), None);
    }

    #[test]
    fn route_schema_is_stable() {
        assert_eq!(PHASE_ROUTE_SCHEMA, "rapidlm.llm.phase_route.v1");
    }
}
