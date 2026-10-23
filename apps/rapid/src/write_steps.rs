//! What happens to a file as the agent writes it, beyond writing it.
//!
//! The steps are configured per project in `.rapidlm/settings.json` and shared,
//! as one value, by a turn's tools and the tools of every subagent it spawns,
//! so a delegated write is treated like the parent's own.
//!
//! Today that is formatting ([`crate::formatters`]).

use std::path::Path;
use std::sync::Arc;

use crate::formatters::{self, Formatters};

/// The configured write steps. Cheap to clone: the configuration is shared.
#[derive(Clone, Debug)]
pub struct WriteSteps(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    formatters: Option<Formatters>,
}

impl WriteSteps {
    /// Parse a settings document. `None` when it configures no step at all;
    /// the second value lists entries that were dropped and why.
    #[must_use]
    pub fn parse(settings: &serde_json::Value) -> (Option<Self>, Vec<String>) {
        let (formatters, problems) = Formatters::parse(settings);
        let steps = formatters
            .is_some()
            .then(|| Self(Arc::new(Inner { formatters })));
        (steps, problems)
    }

    /// Run the formatter that claims `relative`, if any, over `content`.
    #[must_use]
    pub fn format(&self, root: &Path, relative: &str, content: &[u8]) -> formatters::Outcome {
        match &self.0.formatters {
            Some(formatters) => formatters.format(root, relative, content),
            None => formatters::Outcome::NotApplicable,
        }
    }
}
