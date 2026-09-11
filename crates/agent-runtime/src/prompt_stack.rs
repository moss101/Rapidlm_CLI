//! Dynamic system-prompt stack: a template renderer with conditional
//! sections (environment, trust posture, token budget, post-compaction
//! continuation) plus the short post-compaction system prompt.
//!
//! Rendered output is deterministic for a given context: fixed sections in a
//! fixed order, each conditional section included only when its input is
//! present, byte-bounded, with a typed error when a bound is exceeded —
//! nothing is silently truncated. The composed text feeds the context packet
//! as a system block; project rules stay in their own lower-authority slot.

use std::fmt;

/// The short system prompt swapped in after compaction: the session history
/// is a summary, so the model is told to continue without expecting detail.
pub const POST_COMPACTION_SYSTEM_PROMPT: &str = "\
You are continuing a session after context compaction. The earlier conversation \
is available only as a summary. Treat the summary as factual context, not as a \
transcript: re-read any file before editing it, re-run the cheapest check that \
confirms an assumption, and never claim an earlier verification result you \
cannot see. Continue the task from the summary without restating it.";

/// Hard byte cap for one environment section's text.
pub const MAX_ENVIRONMENT_BYTES: usize = 2 * 1024;
/// Hard byte cap for the whole rendered system prompt.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 32 * 1024;
/// Hard byte cap for one token-budget pair as text (defensive; values are u32).
const MAX_BUDGET_SECTION_BYTES: usize = 256;

/// Trust posture of the workspace the session runs in.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TrustPosture {
    /// The project is trusted: workspace-mutating tools may run.
    Trusted,
    /// The project is not trusted: every tool call is refused fail-closed.
    Untrusted,
}

impl TrustPosture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

/// Conditional inputs for one rendered system prompt. Absent inputs leave
/// their section out entirely (presence/absence is the compatibility
/// contract; text is stable for a version).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PromptContext {
    environment: Option<String>,
    trust: Option<TrustPosture>,
    token_budget: Option<(u32, u32)>,
    post_compaction: bool,
}

/// Typed render failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptStackError {
    EnvironmentTooLarge,
    PromptTooLarge,
}

impl PromptStackError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnvironmentTooLarge => "environment text exceeds the section bound",
            Self::PromptTooLarge => "rendered system prompt exceeds the byte bound",
        }
    }
}

impl fmt::Display for PromptStackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PromptStackError {}

impl PromptContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Environment facts (cwd, OS) rendered in the `# Environment` section.
    /// Bounded; an empty string is treated as absent.
    pub fn with_environment(
        mut self,
        environment: impl Into<String>,
    ) -> Result<Self, PromptStackError> {
        let environment = environment.into();
        if environment.is_empty() {
            self.environment = None;
        } else if environment.len() > MAX_ENVIRONMENT_BYTES {
            return Err(PromptStackError::EnvironmentTooLarge);
        } else {
            self.environment = Some(environment);
        }
        Ok(self)
    }

    /// Workspace trust posture section.
    pub fn with_trust(mut self, trust: TrustPosture) -> Self {
        self.trust = Some(trust);
        self
    }

    /// Token-budget section: `(context window tokens, output reserve tokens)`.
    /// Zero values are treated as absent rather than as an empty budget.
    pub fn with_token_budget(mut self, context_limit: u32, output_reserve: u32) -> Self {
        if context_limit == 0 {
            self.token_budget = None;
        } else {
            self.token_budget = Some((context_limit, output_reserve));
        }
        self
    }

    /// Mark the session as post-compaction (adds the continuation section).
    pub fn post_compaction(mut self) -> Self {
        self.post_compaction = true;
        self
    }

    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }

    pub const fn trust(&self) -> Option<TrustPosture> {
        self.trust
    }

    pub const fn token_budget(&self) -> Option<(u32, u32)> {
        self.token_budget
    }

    pub const fn is_post_compaction(&self) -> bool {
        self.post_compaction
    }
}

/// Render the system prompt for `context`. Deterministic: section order and
/// text are fixed, so equal contexts render byte-identical prompts (stable
/// for provider prompt caching).
pub fn render_system_prompt(context: &PromptContext) -> Result<String, PromptStackError> {
    let mut prompt = String::with_capacity(1024);
    prompt.push_str("# Session operating context\n");
    if let Some(environment) = context.environment.as_deref() {
        prompt.push_str("\n## Environment\n");
        prompt.push_str(environment);
        prompt.push('\n');
    }
    if let Some(trust) = context.trust {
        prompt.push_str("\n## Workspace trust\n");
        prompt.push_str(match trust {
            TrustPosture::Trusted => {
                "This workspace is trusted: workspace tools may read and modify \
                 files inside the workspace root. Stay inside that root.\n"
            }
            TrustPosture::Untrusted => {
                "This workspace is NOT trusted: every tool call is refused. \
                 Answer from the conversation only; do not promise file changes.\n"
            }
        });
    }
    if let Some((context_limit, output_reserve)) = context.token_budget {
        let section = format!(
            "\n## Token budget\nContext window: {context_limit} tokens. Reserve \
             {output_reserve} tokens for your final answer. Search before reading, \
             keep quotes short, and prefer paginated reads.\n"
        );
        if section.len() > MAX_BUDGET_SECTION_BYTES {
            return Err(PromptStackError::PromptTooLarge);
        }
        prompt.push_str(&section);
    }
    if context.post_compaction {
        prompt.push_str("\n## After compaction\n");
        prompt.push_str(POST_COMPACTION_SYSTEM_PROMPT);
        prompt.push('\n');
    }
    if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(PromptStackError::PromptTooLarge);
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_renders_only_the_fixed_header() {
        let prompt = render_system_prompt(&PromptContext::new()).expect("render");
        assert!(prompt.starts_with("# Session operating context"));
        assert!(!prompt.contains("## Environment"));
        assert!(!prompt.contains("## Workspace trust"));
        assert!(!prompt.contains("## Token budget"));
        assert!(!prompt.contains("## After compaction"));
    }

    #[test]
    fn conditional_sections_appear_exactly_when_present() {
        let full = render_system_prompt(
            &PromptContext::new()
                .with_environment("cwd /repo; os macos")
                .expect("env")
                .with_trust(TrustPosture::Trusted)
                .with_token_budget(32_768, 4_096)
                .post_compaction(),
        )
        .expect("render");
        assert!(full.contains("## Environment\ncwd /repo; os macos\n"));
        assert!(full.contains("## Workspace trust\nThis workspace is trusted"));
        assert!(full.contains("## Token budget\nContext window: 32768 tokens"));
        assert!(full.contains("Reserve 4096 tokens"));
        assert!(full.contains("## After compaction"));
        assert!(full.contains(POST_COMPACTION_SYSTEM_PROMPT));

        // Absent inputs leave their sections out entirely.
        let partial = render_system_prompt(
            &PromptContext::new()
                .with_trust(TrustPosture::Untrusted)
                .with_token_budget(8_192, 0),
        )
        .expect("render");
        assert!(!partial.contains("## Environment"));
        assert!(partial.contains("## Workspace trust\nThis workspace is NOT trusted"));
        assert!(partial.contains("## Token budget"));
        assert!(!partial.contains("## After compaction"));
    }

    #[test]
    fn zero_token_budget_is_absent_not_empty() {
        let prompt = render_system_prompt(&PromptContext::new().with_token_budget(0, 4_096))
            .expect("render");
        assert!(!prompt.contains("## Token budget"));
    }

    #[test]
    fn rendering_is_deterministic_for_equal_contexts() {
        let build = || {
            render_system_prompt(
                &PromptContext::new()
                    .with_environment("cwd /repo")
                    .expect("env")
                    .with_trust(TrustPosture::Trusted)
                    .with_token_budget(16_384, 2_048),
            )
            .expect("render")
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn oversized_environment_fails_typed() {
        let err = PromptContext::new()
            .with_environment("x".repeat(MAX_ENVIRONMENT_BYTES + 1))
            .expect_err("too large");
        assert_eq!(err, PromptStackError::EnvironmentTooLarge);
        // At the bound it still renders.
        PromptContext::new()
            .with_environment("x".repeat(MAX_ENVIRONMENT_BYTES))
            .expect("at bound");
    }

    #[test]
    fn rendered_prompt_stays_under_its_bound() {
        let prompt = render_system_prompt(
            &PromptContext::new()
                .with_environment("y".repeat(MAX_ENVIRONMENT_BYTES))
                .expect("env")
                .with_trust(TrustPosture::Trusted)
                .with_token_budget(u32::MAX, u32::MAX)
                .post_compaction(),
        )
        .expect("render");
        assert!(prompt.len() <= MAX_SYSTEM_PROMPT_BYTES);
        assert!(!prompt.contains('\u{fffd}'));
    }

    #[test]
    fn post_compaction_prompt_is_short_actionable_and_stable() {
        // "Short" is a design constraint: it must stay well under the
        // per-section budget so the post-compact request stays small.
        assert!(POST_COMPACTION_SYSTEM_PROMPT.len() < MAX_ENVIRONMENT_BYTES);
        assert!(POST_COMPACTION_SYSTEM_PROMPT.contains("summary"));
        assert!(POST_COMPACTION_SYSTEM_PROMPT.contains("re-read"));
    }
}
