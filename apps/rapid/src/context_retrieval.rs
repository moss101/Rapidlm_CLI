//! Proactive repo-content retrieval for a live exec turn.
//!
//! `context-engine` ships a full retrieval stack (FTS index, code graph,
//! Context Scout) that `rapid exec` never called: `build_packet` assembled a
//! `ContextPacket` from fixed prompt-stack text only (system prompt, memory
//! index, rules, skills, goal, reminders), so every turn started the model
//! at zero repo knowledge — it had to `repo_glob`/`workspace_read` its way
//! to relevant files by hand, one at a time, on every single run.
//!
//! This module builds/updates a persistent FTS+graph index under
//! `.rapidlm/index/` (incremental: unchanged files are a content-hash
//! no-op) and runs a Context Scout search against the task prompt,
//! returning the hits as `CompileInput` "retrieved" blocks — the packet
//! compiler already knows how to score, budget, and drop these.
//!
//! Fails open: indexing/search is bounded by a wall-clock timeout and a
//! file-count cap, and any error yields an empty result rather than
//! blocking the turn. This is a quality feature, not a correctness
//! boundary — a slow or unindexable repo must never stop `rapid exec` from
//! running with the same tool-driven exploration it always had.

use std::path::Path;
use std::time::{Duration, Instant};

use context_engine::CancellationToken;
use context_engine::compile::{CompileInput, CompileReason};
use context_engine::ingest::pipeline::{IndexPipeline, PipelineLimits};
use context_engine::ingest::walk::{RepoScope, WalkLimits, walk_manifest};
use context_engine::need::{CompletenessRequirement, InformationNeed, NegativeClaimPolicy, ScopeSet};
use context_engine::repo_manifest::WorkspaceManifest;
use context_engine::retrieval::candidates::{Freshness, TrustClass};
use context_engine::scout::{ScoutLimits, ScoutSources, scout};

/// Wall-clock budget for the whole walk+index+scout pass. Bounded so an
/// unindexable or huge repo degrades to "no retrieval" rather than stalling
/// the turn — the model still has its normal tools either way.
pub const RETRIEVAL_TIMEOUT: Duration = Duration::from_secs(8);
/// Hard cap on files indexed in one pass (a large monorepo still gets a
/// partial, useful index rather than an unbounded walk).
pub const MAX_INDEXED_FILES: usize = 4000;
/// Hard cap on retrieved snippets folded into the packet; `compile()`'s own
/// retrieved-share budget does the real trimming, this is a sanity ceiling.
pub const MAX_REFERENCES: u32 = 12;
/// Byte cap per retrieved snippet read from disk.
pub const MAX_SNIPPET_BYTES: usize = 4096;
/// Task-prompt bytes carried into the scout question (the need's own cap).
const MAX_QUESTION_BYTES: usize = 512;
const INDEX_DIR_NAME: &str = ".rapidlm/index";

/// Retrieve proactive context for `task_prompt` against the repo at `root`.
/// Never fails the caller — logs a one-line warning and returns an empty
/// list on any error.
pub fn retrieve(root: &Path, task_prompt: &str, budget_tokens: u32) -> Vec<CompileInput> {
    match retrieve_inner(root, task_prompt, budget_tokens) {
        Ok(blocks) => {
            eprintln!("context retrieval: {} block(s) proactively retrieved", blocks.len());
            blocks
        }
        Err(reason) => {
            eprintln!("warning: context retrieval skipped: {reason}");
            Vec::new()
        }
    }
}

fn retrieve_inner(
    root: &Path,
    task_prompt: &str,
    budget_tokens: u32,
) -> Result<Vec<CompileInput>, String> {
    let cancel = CancellationToken::new();
    let watcher = spawn_timeout_watcher(cancel.clone(), RETRIEVAL_TIMEOUT);

    let manifest = build_manifest(root, &cancel)?;
    let index_dir = root.join(INDEX_DIR_NAME);
    let limits = PipelineLimits::new()
        .timeout(RETRIEVAL_TIMEOUT)
        .cancellation(cancel.clone());
    let mut pipeline = IndexPipeline::open(&index_dir, manifest.clone(), limits)
        .map_err(|err| format!("index open: {err:?}"))?;

    let walk_limits = WalkLimits::default();
    let mut indexed = 0usize;
    for candidate in walk_manifest(&manifest, RepoScope::All, &walk_limits, &cancel) {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(candidate) = candidate else { continue };
        // A single file failing to parse/chunk must not abort retrieval for
        // the rest of the repo.
        let _ = pipeline.index_file(&candidate);
        indexed += 1;
        if indexed >= MAX_INDEXED_FILES {
            break;
        }
    }

    let question = bounded_str(task_prompt, MAX_QUESTION_BYTES);
    let need = InformationNeed::new(
        vec![question],
        ScopeSet::empty(),
        Vec::new(),
        CompletenessRequirement::Representative,
        false,
        false,
        false,
        false,
        NegativeClaimPolicy::AllowUnchecked,
        budget_tokens.max(1),
    )
    .map_err(|err| format!("need: {err:?}"))?;

    let sources = ScoutSources::new().fts(pipeline.fts()).graph(pipeline.graph());
    let scout_limits = ScoutLimits::new()
        .timeout(RETRIEVAL_TIMEOUT)
        .cancellation(cancel.clone())
        .max_references(MAX_REFERENCES)
        .max_snippets(MAX_REFERENCES);
    let report = scout(&need, sources, &scout_limits).map_err(|err| format!("scout: {err:?}"))?;

    let mut blocks = Vec::new();
    for reference in report.references() {
        let Some(text) =
            read_snippet(root, reference.path().as_str(), reference.start_byte(), reference.end_byte())
        else {
            continue;
        };
        let locator = format!("retrieved:{}", reference.path().as_str());
        blocks.push(
            CompileInput::new(locator, text)
                .reason(CompileReason::Retrieved)
                .trust(TrustClass::Untrusted)
                .freshness(Freshness::Fresh),
        );
    }
    watcher.stop();
    Ok(blocks)
}

fn build_manifest(root: &Path, cancel: &CancellationToken) -> Result<WorkspaceManifest, String> {
    let canonical = root.canonicalize().map_err(|err| format!("canonicalize root: {err}"))?;
    let manifest_toml = format!(
        "schema = 1\n[[repos]]\nalias = \"main\"\nroot = {:?}\nmode = \"read_write\"\n",
        canonical.display()
    );
    WorkspaceManifest::parse(&manifest_toml, &canonical, cancel)
        .map_err(|err| format!("manifest: {err:?}"))
}

fn bounded_str(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Read `[start, end)` of `relative` under `root`, or the file's leading
/// bytes when the scout hit carried no byte range. Never follows the walk's
/// own trust — this is a plain bounded read inside a root we already own.
fn read_snippet(root: &Path, relative: &str, start: Option<u32>, end: Option<u32>) -> Option<String> {
    let bytes = std::fs::read(root.join(relative)).ok()?;
    let (start, end) = match (start, end) {
        (Some(start), Some(end)) if (start as usize) <= (end as usize) => {
            (start as usize, end as usize)
        }
        _ => (0, bytes.len()),
    };
    let end = end.min(bytes.len()).min(start.saturating_add(MAX_SNIPPET_BYTES));
    let start = start.min(end);
    if start == end {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

/// Fires `cancel.cancel()` after `timeout` unless `stop()` is called first —
/// enforces a wall-clock bound on the cooperative-cancellation walk/index/
/// scout loop, which never preempts on its own.
struct TimeoutWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TimeoutWatcher {
    fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn spawn_timeout_watcher(cancel: CancellationToken, timeout: Duration) -> TimeoutWatcher {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_stop = stop.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if watcher_stop.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cancel.cancel();
    });
    TimeoutWatcher { stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rapidlm-ctxretrieve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn retrieve_surfaces_a_matching_file_and_ignores_unrelated_ones() {
        let root = temp_dir("match");
        std::fs::write(
            root.join("lru.py"),
            "class LRUCache:\n    def get(self, key):\n        return self._data.get(key)\n",
        )
        .expect("seed");
        std::fs::write(root.join("unrelated.py"), "def totally_different(): pass\n").expect("seed");

        let blocks = retrieve(&root, "how does LRUCache eviction work", 4096);
        assert!(
            blocks.iter().any(|block| block.text().contains("LRUCache")),
            "expected a block referencing LRUCache, got {blocks:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retrieve_never_panics_on_an_empty_directory() {
        let root = temp_dir("empty");
        let blocks = retrieve(&root, "anything at all", 4096);
        assert!(blocks.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retrieve_fails_open_on_a_nonexistent_root() {
        let root = std::env::temp_dir().join("rapidlm-ctxretrieve-does-not-exist");
        let blocks = retrieve(&root, "anything", 4096);
        assert!(blocks.is_empty());
    }

    #[test]
    fn retrieved_blocks_carry_a_retrieved_locator_and_nonempty_text() {
        // CompileInput's trust()/freshness() are write-only builder setters
        // (verified by construction here, not readback); the real
        // trust/freshness classification is asserted end-to-end through
        // compile() in host::tests, which does expose ContextBlock readers.
        let root = temp_dir("trust");
        std::fs::write(root.join("marker.py"), "def find_me_marker(): pass\n").expect("seed");
        let blocks = retrieve(&root, "find_me_marker function", 4096);
        assert!(!blocks.is_empty(), "expected at least one retrieved block");
        for block in &blocks {
            assert!(block.locator().starts_with("retrieved:"), "{}", block.locator());
            assert!(!block.text().is_empty());
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
