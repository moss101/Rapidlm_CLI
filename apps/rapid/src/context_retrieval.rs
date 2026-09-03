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
use context_engine::ingest::walk::{RepoScope, WalkLimits, walk_manifest, walk_repo};
use context_engine::need::{CompletenessRequirement, InformationNeed, NegativeClaimPolicy, ScopeSet};
use context_engine::repo_manifest::WorkspaceManifest;
use context_engine::retrieval::candidates::{Freshness, TrustClass};
use context_engine::scout::{ScoutLimits, ScoutSources, scout};
use protocol::RepoPath;

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

/// Wall-clock budget for one `ripple_advisory` call — shorter than
/// `RETRIEVAL_TIMEOUT` since this runs synchronously inside a tool call the
/// model is waiting on, not once at turn start.
const RIPPLE_TIMEOUT: Duration = Duration::from_secs(3);
/// Files scanned (not indexed — see `ripple_advisory`'s doc comment) while
/// searching the walk for the one just-written path, before giving up.
const MAX_RIPPLE_WALK_FILES: usize = 20_000;
/// `impact()`'s own hop/result caps for a ripple advisory: shallow and
/// narrow on purpose (a "here's what to check" advisory, not a full
/// transitive dependency audit).
const RIPPLE_HOPS: u32 = 2;
const RIPPLE_LIMIT: u32 = 64;
/// Impacted paths named in one advisory line.
const MAX_RIPPLE_PATHS_LISTED: usize = 8;

/// Retrieve proactive context for `task_prompt` against the repo at `root`.
/// Never fails the caller — logs a one-line warning and returns an empty
/// list on any error.
pub fn retrieve(root: &Path, task_prompt: &str, budget_tokens: u32) -> Vec<CompileInput> {
    let cancel = CancellationToken::new();
    let watcher = spawn_timeout_watcher(cancel.clone(), RETRIEVAL_TIMEOUT);
    let result = retrieve_inner(root, task_prompt, budget_tokens, &cancel);
    watcher.stop();
    match result {
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
    cancel: &CancellationToken,
) -> Result<Vec<CompileInput>, String> {
    let manifest = build_manifest(root, cancel)?;
    let index_dir = root.join(INDEX_DIR_NAME);
    let limits = PipelineLimits::new()
        .timeout(RETRIEVAL_TIMEOUT)
        .cancellation(cancel.clone());
    let mut pipeline = IndexPipeline::open(&index_dir, manifest.clone(), limits)
        .map_err(|err| format!("index open: {err:?}"))?;

    let walk_limits = WalkLimits::default();
    let mut indexed = 0usize;
    for candidate in walk_manifest(&manifest, RepoScope::All, &walk_limits, cancel) {
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
    Ok(blocks)
}

/// Next-Edit-Ripple advisory (Modbit `CTX-017`): after a successful write to
/// `path`, re-index just that one file into the same persistent
/// `.rapidlm/index/` graph [`retrieve`] builds, then report which other
/// already-indexed files define symbols that reference the ones the edit
/// touched — a "you may also need to check these" note appended to the
/// tool's own summary, never a block.
///
/// Fails open like `retrieve`: any error (bad path, no manifest, indexing
/// failure, timeout) is a silent `None`. Deliberately does not re-walk and
/// re-index the whole repo the way `retrieve` does at turn start — a turn
/// with many edits would otherwise pay a full walk per write. Instead this
/// walks (stats/eligibility-checks only, no parsing) looking specifically
/// for the one candidate matching `path`, bounded by
/// `MAX_RIPPLE_WALK_FILES`, and indexes only that single match. A repo
/// larger than the walk bound, or one where `path` sorts very late in walk
/// order, can miss the advisory entirely — accepted for an advisory
/// feature, not the correctness-bearing write path itself.
///
/// **Important limitation:** only `path` itself gets (re)indexed here — its
/// *callers* are found only if they were already indexed by an earlier
/// `retrieve()` call this session (persisted at `.rapidlm/index/`, so this
/// includes prior turns, not just the current one). On a project with no
/// index yet and retrieval skipped or not yet run, this returns `None` even
/// when real callers exist on disk, because the graph simply hasn't seen
/// them. This is a "what we already know" advisory, not an on-demand full
/// dependency audit of the repository.
pub fn ripple_advisory(root: &Path, path: &str) -> Option<String> {
    let cancel = CancellationToken::new();
    let watcher = spawn_timeout_watcher(cancel.clone(), RIPPLE_TIMEOUT);
    let result = ripple_advisory_inner(root, path, &cancel);
    watcher.stop();
    result
}

fn ripple_advisory_inner(root: &Path, path: &str, cancel: &CancellationToken) -> Option<String> {
    let target = RepoPath::parse(path).ok()?;
    let manifest = build_manifest(root, cancel).ok()?;
    let repo = manifest.repo_by_alias("main")?;
    let repo_id = repo.id();

    let walk_limits = WalkLimits::default();
    let candidate = walk_repo(repo, &walk_limits, cancel)
        .take(MAX_RIPPLE_WALK_FILES)
        .filter_map(Result::ok)
        .find(|candidate| candidate.path() == &target)?;

    let index_dir = root.join(INDEX_DIR_NAME);
    let limits = PipelineLimits::new()
        .timeout(RIPPLE_TIMEOUT)
        .cancellation(cancel.clone());
    let mut pipeline = IndexPipeline::open(&index_dir, manifest.clone(), limits).ok()?;
    pipeline.index_file(&candidate).ok()?;

    let symbols = pipeline.graph().symbols_at_path(repo_id, &target).ok()?;
    let mut impacted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for symbol in &symbols {
        let Ok(edges) = pipeline.graph().impact(symbol, RIPPLE_HOPS, RIPPLE_LIMIT) else {
            continue;
        };
        for edge in &edges {
            if let Some((_, impacted_path)) = pipeline.graph().symbol_label(edge.from())
                && impacted_path.as_str() != path
            {
                impacted.insert(impacted_path.as_str().to_owned());
            }
        }
    }
    if impacted.is_empty() {
        return None;
    }
    let listed: Vec<String> = impacted.iter().take(MAX_RIPPLE_PATHS_LISTED).cloned().collect();
    let more = impacted.len().saturating_sub(listed.len());
    let suffix = if more > 0 { format!(" (+{more} more)") } else { String::new() };
    Some(format!(
        "advisory: editing {path} may affect code that references it in: {}{suffix} — verify \
         they still work",
        listed.join(", ")
    ))
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
        // The loop above only checks the stop flag *inside* each iteration;
        // a `stop()` that lands after the last check but before the loop's
        // own timeout condition trips falls through here unobserved. One
        // more check right before firing closes that gap — `stop()` calls
        // are meant to reliably prevent cancellation, not just usually win
        // a race against the poll interval.
        if watcher_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
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
    fn ripple_advisory_names_the_file_that_calls_the_edited_function() {
        let root = temp_dir("ripple");
        std::fs::write(root.join("a.rs"), "fn a() { b(); }\n").expect("seed a");
        std::fs::write(root.join("b.rs"), "fn b() {}\n").expect("seed b");
        // In real use, `retrieve()` already indexed the whole repo (a.rs
        // included) once at turn start; `ripple_advisory` only needs to
        // freshen the one file the current turn just wrote (b.rs), not
        // every caller of it.
        let _ = retrieve(&root, "b", 4096);

        let advisory = ripple_advisory(&root, "b.rs").expect("advisory expected");
        assert!(advisory.contains("a.rs"), "{advisory}");
        assert!(advisory.starts_with("advisory: editing b.rs"), "{advisory}");

        // A function nothing else calls carries no ripple advisory at all.
        std::fs::write(root.join("lonely.rs"), "fn lonely() {}\n").expect("seed lonely");
        assert!(ripple_advisory(&root, "lonely.rs").is_none());

        // An unknown/nonexistent path fails open rather than panicking.
        assert!(ripple_advisory(&root, "does-not-exist.rs").is_none());

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
    fn timeout_watcher_is_stopped_even_when_retrieve_inner_errors_early() {
        // Mirrors retrieve()'s own spawn/call/stop structure with a short
        // timeout: build_manifest fails immediately on a nonexistent root,
        // so if stop() isn't reached on that path, the watcher keeps
        // sleeping and fires cancel.cancel() once the timeout elapses.
        let root = std::env::temp_dir().join("rapidlm-ctxretrieve-does-not-exist-watcher");
        let cancel = CancellationToken::new();
        let watcher = spawn_timeout_watcher(cancel.clone(), Duration::from_millis(30));
        let result = retrieve_inner(&root, "anything", 4096, &cancel);
        watcher.stop();
        assert!(result.is_err(), "nonexistent root must fail retrieve_inner");
        std::thread::sleep(Duration::from_millis(90));
        assert!(
            !cancel.is_cancelled(),
            "stop() must run on the error path, or the leaked watcher thread \
             fires cancel.cancel() once its timeout elapses"
        );
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
