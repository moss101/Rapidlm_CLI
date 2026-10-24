//! What happens to a file as the agent writes it, beyond writing it.
//!
//! The steps are configured per project in `.rapidlm/settings.json` and shared,
//! as one value, by a turn's tools and the tools of every subagent it spawns, so
//! a delegated write is treated like the parent's own.
//!
//! - formatting ([`crate::formatters`]) rewrites the content before it is written;
//! - language-server diagnostics ([`crate::lsp_diagnostics`]) report problems in
//!   the file once it is written.
//!
//! Tools are built afresh for every turn, but a language server is expensive to
//! start and should live as long as the session, so the pool of servers is kept
//! in an [`LspCache`] the session owns and each turn's steps are attached to.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::formatters::{self, Formatters};
use crate::lsp_diagnostics::{self, LspConfig, LspPool};

/// The session's language servers, reused from turn to turn while the settings
/// that define them stay the same. Dropping the last handle stops the servers.
#[derive(Clone, Default)]
pub struct LspCache(Arc<Mutex<Option<CachedPool>>>);

/// The settings fingerprint a pool was built for, and the pool.
type CachedPool = (String, Arc<LspPool>);

impl LspCache {
    /// Stop the cached servers: language servers were configured and are not any
    /// more.
    pub fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The pool for `config` and `root`, reusing the cached one when both match.
    fn pool(&self, config: &LspConfig, root: &Path) -> Arc<LspPool> {
        let key = format!("{}|{}", root.display(), config.fingerprint());
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, pool)) = slot.as_ref()
            && *cached == key
        {
            return Arc::clone(pool);
        }
        let pool = Arc::new(LspPool::new(config.clone(), root));
        *slot = Some((key, Arc::clone(&pool)));
        pool
    }
}

/// The configured write steps. Cheap to clone: the configuration is shared.
#[derive(Clone, Debug)]
pub struct WriteSteps(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    formatters: Option<Formatters>,
    lsp: Option<LspSlot>,
}

#[derive(Debug)]
enum LspSlot {
    /// Parsed, not yet attached to a session.
    Config(LspConfig),
    /// Attached: servers start on demand in this pool.
    Pool(Arc<LspPool>),
}

impl WriteSteps {
    /// Parse one settings document. `None` when it configures no step at all;
    /// the second value lists entries that were dropped and why.
    #[must_use]
    pub fn parse(settings: &serde_json::Value) -> (Option<Self>, Vec<String>) {
        Self::parse_all(std::slice::from_ref(settings))
    }

    /// Parse several settings documents, most important first. Each kind of step
    /// comes from the first document that configures it, so formatters in one
    /// file and language servers in another both apply.
    #[must_use]
    pub fn parse_all(documents: &[serde_json::Value]) -> (Option<Self>, Vec<String>) {
        let mut problems = Vec::new();
        let mut formatters = None;
        let mut lsp = None;
        for document in documents {
            if formatters.is_none() {
                let (found, issues) = Formatters::parse(document);
                formatters = found;
                problems.extend(issues);
            }
            if lsp.is_none() {
                let (found, issues) = lsp_diagnostics::parse(document);
                lsp = found;
                problems.extend(issues);
            }
        }
        let steps = (formatters.is_some() || lsp.is_some()).then(|| {
            Self(Arc::new(Inner {
                formatters,
                lsp: lsp.map(LspSlot::Config),
            }))
        });
        (steps, problems)
    }

    /// Attach the session's server pool. Without this the formatters still
    /// work and diagnostics are skipped.
    #[must_use]
    pub fn with_session(self, root: &Path, cache: &LspCache) -> Self {
        let lsp = match &self.0.lsp {
            Some(LspSlot::Config(config)) => Some(LspSlot::Pool(cache.pool(config, root))),
            Some(LspSlot::Pool(pool)) => Some(LspSlot::Pool(Arc::clone(pool))),
            None => None,
        };
        Self(Arc::new(Inner {
            formatters: self.0.formatters.clone(),
            lsp,
        }))
    }

    /// Run the formatter that claims `relative`, if any, over `content`.
    #[must_use]
    pub fn format(&self, root: &Path, relative: &str, content: &[u8]) -> formatters::Outcome {
        match &self.0.formatters {
            Some(formatters) => formatters.format(root, relative, content),
            None => formatters::Outcome::NotApplicable,
        }
    }

    /// Whether language servers are configured.
    #[must_use]
    pub fn has_lsp(&self) -> bool {
        self.0.lsp.is_some()
    }

    /// What the language server says about the file `relative` under the
    /// tools' root `tool_root`, whose content is now `content`, as text for a
    /// tool result.
    #[must_use]
    pub fn diagnostics(&self, tool_root: &Path, relative: &str, content: &[u8]) -> Option<String> {
        match &self.0.lsp {
            Some(LspSlot::Pool(pool)) => pool.diagnostics(tool_root, relative, content),
            _ => None,
        }
    }
}
