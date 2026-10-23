//! Turn-level undo: take the files a turn changed back to how they were, and
//! take the conversation back to just before it.
//!
//! # How the files are tracked
//!
//! A private bare git repository records the project's working tree at the
//! start and the end of every turn. Only tree objects are written, never
//! commits, and the repository's work tree is the project itself, so what is
//! recorded is what `git add -A` would see: the project's own ignore rules
//! apply, build output and dependencies are left out, and a change made by a
//! shell command is captured exactly like one made by a file tool. Content
//! filters and line-ending conversion are switched off for this repository, so
//! what is stored is the bytes on disk.
//!
//! The repository lives in the user's RapidLM home, keyed by the project's
//! path, **not** inside the project. A project is untrusted input: one that
//! shipped its own store could carry git hooks or configuration that run the
//! first time the store is used. Nothing in the project is ever read as git
//! configuration for this store, and hooks are switched off besides.
//!
//! # What undo promises
//!
//! Restoring a turn means moving each file the turn changed from its end state
//! back to its start state. A file is moved only if it is **still as the turn
//! left it** (content and, where the platform has them, the executable bit),
//! checked again immediately before it is written. A file edited since, by the
//! person or by a later tool, is a conflict, and a conflict stops the whole undo
//! unless it is forced, so work done after the turn is never overwritten by
//! accident. Redo is the same move in the other direction, with the same check.
//!
//! If the turn changed a `.gitignore` that existed before it, files the turn
//! "created" cannot be told apart from files that were merely ignored before,
//! so removing them is treated as a conflict too.
//!
//! Symbolic links and nested repositories are not restorable and are reported.
//! Files the project ignores are not tracked, so an ignored file the agent
//! edited stays as it is.
//!
//! The module knows nothing about sessions' event ledgers; the interactive loop
//! does the conversation half (forking the session the turn was recorded in
//! back to the sequence this module kept) and calls in here for the files.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Subdirectory of the RapidLM home that holds every project's store.
pub const DIR: &str = "undo";

/// Environment variable that turns tracking off (`RAPIDLM_NO_UNDO=1`). A very
/// large working tree makes two `git add -A` passes per turn a real cost.
pub const DISABLE_ENV: &str = "RAPIDLM_NO_UNDO";

/// Most turns remembered per session.
pub const MAX_ENTRIES: usize = 50;

/// How long an idle session's journal is kept before pruning.
const JOURNAL_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Unreferenced objects younger than this survive a prune. A tree written by
/// another process a moment ago, not yet named by its journal, must not go.
const PRUNE_GRACE: &str = "2.hours.ago";

/// Ref namespace pinning every recorded tree, so git never collects one that
/// a journal still names.
const REF_PREFIX: &str = "refs/rapid/";

/// Largest file `redo`/`undo` will read back from the store into memory.
const MAX_RESTORE_BYTES: usize = 256 * 1024 * 1024;

/// Largest file read to compare it with what the turn left.
const MAX_COMPARE_BYTES: u64 = 1024 * 1024 * 1024;

/// Whether tracking is switched off by the environment.
#[must_use]
pub fn disabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Where the store and journals for the project at `root` live, under the
/// RapidLM `home`.
#[must_use]
pub fn store_dir(home: &Path, root: &Path) -> PathBuf {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let key: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    home.join(DIR).join(key)
}

/// An error from the store, already worded for a person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndoError(pub String);

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UndoError {}

impl From<io::Error> for UndoError {
    fn from(err: io::Error) -> Self {
        Self(err.to_string())
    }
}

type Result<T> = std::result::Result<T, UndoError>;

fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(UndoError(message.into()))
}

/// The identity of a recorded working tree (a git tree object id).
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TreeId(String);

impl TreeId {
    /// The id as hexadecimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One file's recorded content: a git blob and its mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Blob {
    oid: String,
    mode: u32,
}

impl Blob {
    fn is_regular(&self) -> bool {
        matches!(self.mode, 0o100_644 | 0o100_755)
    }

    fn is_executable(&self) -> bool {
        self.mode == 0o100_755
    }
}

/// Why a file cannot be moved without losing something.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Conflict {
    /// The file differs from what the turn left (its content, or its
    /// executable bit).
    Modified,
    /// The turn left no file here, and one exists now.
    Appeared,
    /// The turn left a file here, and it is gone now.
    Vanished,
    /// A link or other non-file sits at the path.
    NotAFile,
    /// A `.gitignore` the project already had was changed by the turn, so it
    /// is unclear that the turn created this file.
    IgnoreRulesChanged,
}

impl Conflict {
    fn describe(&self) -> &'static str {
        match self {
            Self::Modified => "changed since",
            Self::Appeared => "created since",
            Self::Vanished => "deleted since",
            Self::NotAFile => "is not a regular file now",
            Self::IgnoreRulesChanged => {
                "a .gitignore changed in that turn, so it may not have created this"
            }
        }
    }
}

/// One path a plan moves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    /// Path relative to the project root, with `/` separators.
    pub path: String,
    /// What the file is expected to be now (`None`: absent).
    expected: Option<Blob>,
    /// What it should become (`None`: absent).
    target: Option<Blob>,
    /// Set when the file is not as expected.
    pub conflict: Option<Conflict>,
    /// Set when the file already is what it should become.
    pub already: bool,
    /// Set when removing this file needs the ignore-rule caution.
    guarded: bool,
}

impl Item {
    /// What the move does to the file.
    #[must_use]
    pub fn action(&self) -> &'static str {
        match (&self.expected, &self.target) {
            (None, Some(_)) => "restore",
            (Some(_), None) => "remove",
            _ => "revert",
        }
    }
}

/// The moves needed to take the project from one recorded tree to another.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Plan {
    /// Every regular-file move, conflicting or not.
    pub items: Vec<Item>,
    /// Paths that cannot be moved at all (links, nested repositories).
    pub unsupported: Vec<String>,
}

impl Plan {
    /// Items whose file is not as expected.
    pub fn conflicts(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|item| item.conflict.is_some())
    }

    /// Whether any file is not as expected.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.conflicts().next().is_some()
    }

    /// Items that will change a file.
    pub fn pending(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|item| !item.already)
    }

    /// Whether the move changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending().next().is_none()
    }

    fn paths(&self) -> Vec<String> {
        self.items.iter().map(|i| i.path.clone()).collect()
    }
}

/// What applying a plan did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Applied {
    /// Files written back.
    pub written: Vec<String>,
    /// Files removed.
    pub removed: Vec<String>,
    /// Files left because they conflicted and the move was not forced.
    pub kept: Vec<String>,
    /// Files that could not be moved, with the reason.
    pub failed: Vec<(String, String)>,
}

/// The bare repository and the working tree it watches.
pub struct Store {
    git_dir: PathBuf,
    root: PathBuf,
}

/// Variables that would point git somewhere other than the store, or add
/// configuration the store did not ask for.
const GIT_ENV_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_ATTR_SOURCE",
    "GIT_ATTR_NOSYSTEM",
    "GIT_EXCLUDE_FILE",
    "GIT_TEMPLATE_DIR",
];

/// The store's own ignore rules. The project's `.rapidlm/` holds runtime state
/// (the session database, locks); only its hand-written parts are work an
/// agent might change and a person might want back.
const EXCLUDES: &str = "/.rapidlm/*\n!/.rapidlm/commands/\n!/.rapidlm/MEMORY.md\n";

/// Attributes that switch off every content transformation, so a stored blob is
/// the file's bytes. They outrank the project's own `.gitattributes`.
const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding -eol\n";

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Store {
    /// Open the store in `base`, creating it on first use, for the project at
    /// `root`.
    ///
    /// # Errors
    /// When `git` is unavailable or the store cannot be created.
    pub fn open(root: &Path, base: &Path) -> Result<Self> {
        let git_dir = base.join("store.git");
        let store = Self {
            git_dir,
            root: root.to_path_buf(),
        };
        if !store.git_dir.join("HEAD").is_file() {
            fs::create_dir_all(base)?;
            let mut command = Command::new("git");
            command
                .arg("init")
                .arg("--bare")
                .arg("--template=")
                .arg("-q")
                .arg(&store.git_dir)
                .stdin(Stdio::null());
            for name in GIT_ENV_REMOVED {
                command.env_remove(name);
            }
            let output = command
                .output()
                .map_err(|e| UndoError(format!("git is required for undo: {e}")))?;
            if !output.status.success() {
                return err(format!(
                    "could not create the undo store: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        let info = store.git_dir.join("info");
        fs::create_dir_all(&info)?;
        // The project's own `.git/info/exclude` is where people keep rules for
        // files that are theirs alone (`.env.local`, scratch data). Honour it:
        // the store would otherwise copy those files into itself.
        let mut excludes = EXCLUDES.to_owned();
        if let Ok(personal) = fs::read_to_string(root.join(".git").join("info").join("exclude")) {
            excludes.push_str(&personal);
            if !excludes.ends_with('\n') {
                excludes.push('\n');
            }
        }
        write_if_different(&info.join("exclude"), &excludes)?;
        write_if_different(&info.join("attributes"), ATTRIBUTES)?;
        Ok(store)
    }

    fn git(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>> {
        let hooks = format!(
            "core.hooksPath={}",
            self.git_dir.join("hooks-off").display()
        );
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.safecrlf=false",
                "-c",
                "core.quotepath=off",
                "-c",
                "core.longpaths=true",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "gc.auto=0",
                "-c",
                "safe.directory=*",
                "-c",
            ])
            .arg(&hooks)
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.root)
            .args(args)
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        for name in GIT_ENV_REMOVED {
            command.env_remove(name);
        }
        let mut child = command
            .spawn()
            .map_err(|e| UndoError(format!("git could not run: {e}")))?;
        // Written from a thread: a child that fills its output pipe before it
        // has read all of its input would otherwise deadlock against us.
        let writer = stdin.and_then(|bytes| {
            child.stdin.take().map(|mut pipe| {
                std::thread::spawn(move || {
                    let _ = pipe.write_all(&bytes);
                })
            })
        });
        let output = child
            .wait_with_output()
            .map_err(|e| UndoError(format!("git failed: {e}")))?;
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        if !output.status.success() {
            return err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    }

    /// Record the project's working tree as it is now.
    ///
    /// # Errors
    /// When git fails.
    pub fn snapshot(&self) -> Result<TreeId> {
        // `--ignore-errors` records everything readable instead of giving up
        // at the first unreadable file. git still exits non-zero for such a
        // file; only that case is tolerated, never a failure to write the index.
        if let Err(e) = self.git(&["add", "-A", "--ignore-errors"], None) {
            let unreadable = (e.0.contains("unable to index file") || e.0.contains("open("))
                && !e.0.contains("index file.lock")
                && !e.0.contains("new index file")
                && !e.0.contains("No space left");
            if !unreadable {
                return Err(e);
            }
        }
        let out = self.git(&["write-tree"], None)?;
        let id = String::from_utf8_lossy(&out).trim().to_owned();
        if !is_oid(&id) {
            return err("git did not return a tree id");
        }
        let tree = TreeId(id);
        self.pin(&[&tree])?;
        Ok(tree)
    }

    /// Pin trees with refs. A ref may point at any object, and an unpinned tree
    /// is unreachable and so eligible for collection. One git process, whatever
    /// the number of trees.
    fn pin(&self, trees: &[&TreeId]) -> Result<()> {
        if trees.is_empty() {
            return Ok(());
        }
        let mut script = String::new();
        let mut seen = BTreeSet::new();
        for tree in trees {
            if seen.insert(tree.as_str()) {
                script.push_str(&format!(
                    "update {REF_PREFIX}{} {}\n",
                    tree.as_str(),
                    tree.as_str()
                ));
            }
        }
        self.git(&["update-ref", "--stdin"], Some(script.into_bytes()))?;
        Ok(())
    }

    /// The moves that take a project now matching `expected` to `target`.
    ///
    /// Nothing is changed. Each regular file is checked against the working
    /// tree: it must be as `expected` says (or already be what `target` says),
    /// or it is recorded as a conflict.
    ///
    /// # Errors
    /// When git fails, for instance because a recorded tree was pruned.
    pub fn plan(&self, expected: &TreeId, target: &TreeId) -> Result<Plan> {
        let raw = self.git(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--raw",
                "--no-renames",
                "--no-abbrev",
                expected.as_str(),
                target.as_str(),
            ],
            None,
        )?;
        let mut plan = Plan::default();
        let mut tokens = raw.split(|b| *b == 0).filter(|t| !t.is_empty());
        while let Some(header) = tokens.next() {
            let Some(path) = tokens.next() else { break };
            let (Ok(header), Ok(path)) = (std::str::from_utf8(header), std::str::from_utf8(path))
            else {
                plan.unsupported
                    .push(String::from_utf8_lossy(path).into_owned());
                continue;
            };
            let Some((old, new)) = parse_header(header) else {
                continue;
            };
            if !safe_relative(path) {
                plan.unsupported.push(path.to_owned());
                continue;
            }
            let regular = |b: &Option<Blob>| b.as_ref().is_none_or(Blob::is_regular);
            if !regular(&old) || !regular(&new) {
                plan.unsupported.push(path.to_owned());
                continue;
            }
            plan.items.push(Item {
                path: path.to_owned(),
                expected: old,
                target: new,
                conflict: None,
                already: false,
                guarded: false,
            });
        }
        // A `.gitignore` the project already had, changed by the turn, decides
        // what the "after" tree contains; a file present there and absent
        // before may only have been ignored before.
        let ignore_rules_changed = plan.items.iter().any(|item| {
            item.target.is_some()
                && item
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name == ".gitignore")
        });
        for item in &mut plan.items {
            item.guarded = ignore_rules_changed && item.target.is_none();
        }
        let paths = plan.paths();
        for item in &mut plan.items {
            self.classify(item, &paths)?;
        }
        Ok(plan)
    }

    /// Compare one item's file on disk with what it is expected to be.
    fn classify(&self, item: &mut Item, plan_paths: &[String]) -> Result<()> {
        item.conflict = None;
        item.already = false;
        let on_disk = self.root.join(&item.path);
        match fs::symlink_metadata(&on_disk) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => match (&item.expected, &item.target) {
                (None, _) => {}
                (Some(_), None) => item.already = true,
                (Some(_), Some(_)) => item.conflict = Some(Conflict::Vanished),
            },
            Err(e) => return Err(e.into()),
            Ok(meta) if meta.is_file() => match (&item.expected, &item.target) {
                (None, None) => {}
                (None, Some(_)) => item.conflict = Some(Conflict::Appeared),
                (Some(expected), target) => {
                    let (oid, executable) = self.disk_state(&on_disk, &meta)?;
                    let is = |blob: &Blob| oid == blob.oid && mode_matches(executable, blob);
                    if is(expected) {
                        // As the turn left it: free to move.
                    } else if target.as_ref().is_some_and(is) {
                        item.already = true;
                    } else {
                        item.conflict = Some(Conflict::Modified);
                    }
                }
            },
            // A directory the plan itself is about to empty is not in the way:
            // a turn that replaced a file with a directory is undone by removing
            // the directory's files first and then restoring the file.
            Ok(meta)
                if meta.is_dir()
                    && plan_paths
                        .iter()
                        .any(|p| p.starts_with(&format!("{}/", item.path))) => {}
            Ok(_) => item.conflict = Some(Conflict::NotAFile),
        }
        if item.guarded && item.target.is_none() && !item.already && item.conflict.is_none() {
            item.conflict = Some(Conflict::IgnoreRulesChanged);
        }
        Ok(())
    }

    /// The git blob id of the file's bytes and whether it is executable.
    ///
    /// Hashed with the file's bytes on standard input, never by path: git
    /// reads a path with a leading quote or a newline as something else.
    fn disk_state(&self, path: &Path, meta: &fs::Metadata) -> Result<(String, bool)> {
        if meta.len() > MAX_COMPARE_BYTES {
            return err(format!("{} is too large to compare", path.display()));
        }
        let bytes = fs::read(path)?;
        let out = self.git(&["hash-object", "--no-filters", "--stdin"], Some(bytes))?;
        let oid = String::from_utf8_lossy(&out).trim().to_owned();
        if !is_oid(&oid) {
            return err("git did not return a blob id");
        }
        Ok((oid, is_executable(meta)))
    }

    /// Carry out `plan`. Conflicting files are left alone unless `force`.
    ///
    /// Each file is compared with the turn's end state again just before it is
    /// written, so a change made while the conversation was being forked (a
    /// save from an editor, a background job) is a conflict, not a casualty.
    ///
    /// # Errors
    /// Only when the store itself cannot be read; a file that cannot be moved
    /// is reported in [`Applied::failed`] and the rest are still moved.
    pub fn apply(&self, plan: &Plan, force: bool) -> Result<Applied> {
        let mut applied = Applied::default();
        let paths = plan.paths();
        // Removals first, so a path that was a file and becomes a directory
        // (or the reverse) is clear before anything is written under it.
        let ordered = plan
            .items
            .iter()
            .filter(|i| i.target.is_none())
            .chain(plan.items.iter().filter(|i| i.target.is_some()));
        for item in ordered {
            if item.already {
                continue;
            }
            let mut current = item.clone();
            self.classify(&mut current, &paths)?;
            if current.already {
                continue;
            }
            if current.conflict.is_some() && !force {
                applied.kept.push(item.path.clone());
                continue;
            }
            let on_disk = self.root.join(&item.path);
            match &item.target {
                None => match remove_file_and_empty_parents(&self.root, &on_disk) {
                    Ok(()) => applied.removed.push(item.path.clone()),
                    Err(e) => applied.failed.push((item.path.clone(), e.to_string())),
                },
                Some(blob) => match self.restore(blob, &on_disk) {
                    Ok(()) => applied.written.push(item.path.clone()),
                    Err(e) => applied.failed.push((item.path.clone(), e.to_string())),
                },
            }
        }
        Ok(applied)
    }

    fn restore(&self, blob: &Blob, destination: &Path) -> Result<()> {
        let size = self.git(&["cat-file", "-s", &blob.oid], None)?;
        let size: usize = String::from_utf8_lossy(&size).trim().parse().unwrap_or(0);
        if size > MAX_RESTORE_BYTES {
            return err(format!("{size} bytes is too large to restore in memory"));
        }
        let bytes = self.git(&["cat-file", "blob", &blob.oid], None)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = fs::symlink_metadata(destination).ok();
        if existing.as_ref().is_some_and(fs::Metadata::is_dir) {
            return err("a directory is in the way");
        }
        let name = destination
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unique = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging =
            destination.with_file_name(format!(".{name}.undo-{}-{unique}", std::process::id()));
        let placed = (|| -> Result<()> {
            fs::write(&staging, &bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // A file that is being replaced keeps its permissions, so a
                // private file (`.env`, a key) never comes back world-readable.
                // The executable bit follows the recorded file.
                let base = existing
                    .as_ref()
                    .filter(|m| m.is_file())
                    .map_or(0o644, |m| m.permissions().mode() & 0o777);
                let mode = if blob.is_executable() {
                    base | ((base & 0o444) >> 2)
                } else {
                    base & !0o111
                };
                fs::set_permissions(&staging, fs::Permissions::from_mode(mode))?;
            }
            fs::rename(&staging, destination)?;
            Ok(())
        })();
        if placed.is_err() {
            let _ = fs::remove_file(&staging);
        }
        placed
    }

    /// Drop pins for trees no journal names, and collect them once they have
    /// outlived the grace period.
    fn prune(&self, keep: &BTreeSet<String>) {
        let Ok(refs) = self.git(&["for-each-ref", "--format=%(refname)", REF_PREFIX], None) else {
            return;
        };
        let mut script = String::new();
        for name in String::from_utf8_lossy(&refs).lines() {
            let id = name.trim_start_matches(REF_PREFIX);
            if !keep.contains(id) {
                script.push_str(&format!("delete {name}\n"));
            }
        }
        if script.is_empty() {
            return;
        }
        if self
            .git(&["update-ref", "--stdin"], Some(script.into_bytes()))
            .is_ok()
        {
            let _ = self.git(&["prune", &format!("--expire={PRUNE_GRACE}")], None);
        }
    }
}

fn write_if_different(path: &Path, text: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    fs::write(path, text)
}

fn is_oid(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}

/// Whether a file's executable bit is what `blob` records. Platforms without
/// the bit cannot disagree.
#[cfg(unix)]
fn mode_matches(executable: bool, blob: &Blob) -> bool {
    executable == blob.is_executable()
}

#[cfg(not(unix))]
fn mode_matches(_: bool, _: &Blob) -> bool {
    true
}

/// `:100644 100644 <old> <new> M` into the two sides.
fn parse_header(header: &str) -> Option<(Option<Blob>, Option<Blob>)> {
    let mut fields = header.strip_prefix(':')?.split(' ');
    let old_mode = u32::from_str_radix(fields.next()?, 8).ok()?;
    let new_mode = u32::from_str_radix(fields.next()?, 8).ok()?;
    let old_oid = fields.next()?;
    let new_oid = fields.next()?;
    let side = |mode: u32, oid: &str| {
        (mode != 0 && !oid.bytes().all(|b| b == b'0')).then(|| Blob {
            oid: oid.to_owned(),
            mode,
        })
    };
    Some((side(old_mode, old_oid), side(new_mode, new_oid)))
}

/// A path a plan may touch: relative, no `..`, nothing inside `.git`.
fn safe_relative(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && p.is_relative()
        && p.components()
            .all(|c| matches!(c, Component::Normal(part) if part != ".git"))
}

fn remove_file_and_empty_parents(root: &Path, file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Directories the turn created and the removal emptied go with it; the
    // first non-empty one (or the root) ends the walk.
    let mut dir = file.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The journal: which turns can be undone, per session.
// ---------------------------------------------------------------------------

/// One recorded turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Entry {
    /// What the person typed.
    pub prompt: String,
    /// The session the turn was recorded in. A fork's ledger restarts at
    /// sequence 1, so `seq_before` means something only in this session.
    #[serde(default)]
    pub session: String,
    /// The session's ledger sequence just before the turn was submitted.
    pub seq_before: u64,
    /// The project as the turn found it.
    pub before: TreeId,
    /// The project as the turn left it.
    pub after: TreeId,
}

/// An undone turn that `redo` can bring back.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Undone {
    pub entry: Entry,
    /// The session in which the turn still exists.
    pub session: String,
}

/// A session's undo history, persisted beside the store.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Journal {
    /// Turns, oldest first.
    pub entries: Vec<Entry>,
    /// Turns undone from this session, most recent last.
    pub redo: Vec<Undone>,
    /// The tree a running turn found. Not an entry yet, but it must survive a
    /// prune by another process until the turn is settled.
    #[serde(default)]
    pub inflight: Option<TreeId>,
}

impl Journal {
    fn trees(&self) -> Vec<&TreeId> {
        self.entries
            .iter()
            .chain(self.redo.iter().map(|u| &u.entry))
            .flat_map(|e| [&e.before, &e.after])
            .chain(self.inflight.iter())
            .collect()
    }
}

fn journal_path(base: &Path, session: &str) -> PathBuf {
    base.join("journal").join(format!("{session}.json"))
}

fn session_name_ok(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn load_journal(base: &Path, session: &str) -> Journal {
    let mut journal: Journal = fs::read(journal_path(base, session))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    // Entries written before they named their session belong to this one.
    for entry in &mut journal.entries {
        if entry.session.is_empty() {
            entry.session = session.to_owned();
        }
    }
    for undone in &mut journal.redo {
        if undone.entry.session.is_empty() {
            undone.entry.session = undone.session.clone();
        }
    }
    journal
}

fn save_journal(store: &Store, base: &Path, session: &str, journal: &Journal) -> Result<()> {
    let path = journal_path(base, session);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let unique = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging = path.with_extension(format!("json.{}-{unique}.tmp", std::process::id()));
    fs::write(
        &staging,
        serde_json::to_vec(journal).map_err(|e| UndoError(e.to_string()))?,
    )?;
    fs::rename(&staging, &path)?;
    // Whatever another process's prune did in the meantime, everything this
    // journal names is pinned again.
    store.pin(&journal.trees())
}

/// Every tree any journal names, with journals past their age limit removed.
fn live_trees_after_expiry(base: &Path) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    let Ok(entries) = fs::read_dir(base.join("journal")) else {
        return live;
    };
    for file in entries.filter_map(std::result::Result::ok) {
        let path = file.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let stale = file
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > JOURNAL_MAX_AGE);
        if stale {
            let _ = fs::remove_file(&path);
            continue;
        }
        let Some(journal) = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Journal>(&b).ok())
        else {
            continue;
        };
        live.extend(journal.trees().into_iter().map(|t| t.0.clone()));
    }
    live
}

// ---------------------------------------------------------------------------
// The tracker the interactive loop holds.
// ---------------------------------------------------------------------------

struct Pending {
    prompt: String,
    seq_before: u64,
    before: TreeId,
}

struct Active {
    store: Store,
    base: PathBuf,
    session: String,
    journal: Journal,
    pending: Option<Pending>,
}

/// A step prepared by [`UndoState::prepare_undo`] or
/// [`UndoState::prepare_redo`], not yet applied.
#[derive(Debug)]
pub struct Step {
    /// The turn being moved.
    pub entry: Entry,
    /// The files to move.
    pub plan: Plan,
    /// For a redo: the session that still holds the turn.
    pub session: Option<String>,
}

/// Per-session undo tracking. `Default` is "not started": nothing touches disk
/// until a turn begins.
#[derive(Default)]
pub struct UndoState {
    active: Option<Active>,
    pruned: bool,
    home: Option<PathBuf>,
    warned: bool,
}

impl UndoState {
    /// Start recording turns, keeping the store under the RapidLM `home`. A
    /// state that was never enabled records nothing and touches no disk, which
    /// keeps one-shot helpers and tests inert.
    pub fn enable(&mut self, home: &Path) {
        self.home = Some(home.to_path_buf());
    }

    /// The first time this is called with a reason, the reason back; after
    /// that, `None`. A store that cannot be opened fails every turn the same
    /// way, and saying so once is enough.
    pub fn unavailable_once(&mut self, reason: &str) -> Option<String> {
        if self.warned {
            None
        } else {
            self.warned = true;
            Some(reason.to_owned())
        }
    }

    fn ensure(&mut self, root: &Path, session: &str) -> Result<&mut Active> {
        if !session_name_ok(session) {
            return err("the session id is not usable as a file name");
        }
        let Some(home) = self.home.clone() else {
            return err("undo is not enabled for this session");
        };
        let base = store_dir(&home, root);
        let reload = self
            .active
            .as_ref()
            .is_none_or(|a| a.session != session || a.base != base);
        if reload {
            // A pending snapshot of the previous session is dropped unrecorded;
            // it was never settled, so there is no end state to pair it with.
            let store = match self.active.take() {
                Some(a) if a.base == base => a.store,
                _ => Store::open(root, &base)?,
            };
            if !self.pruned {
                self.pruned = true;
                store.prune(&live_trees_after_expiry(&base));
            }
            self.active = Some(Active {
                store,
                journal: load_journal(&base, session),
                base,
                session: session.to_owned(),
                pending: None,
            });
        }
        self.active
            .as_mut()
            .ok_or_else(|| UndoError("undo state vanished".to_owned()))
    }

    /// A turn is about to be submitted: record the tree it will find.
    ///
    /// # Errors
    /// When the store is unavailable. The turn should run regardless.
    pub fn begin(
        &mut self,
        root: &Path,
        session: &str,
        prompt: &str,
        seq_before: u64,
    ) -> Result<()> {
        if self.home.is_none() || disabled() {
            return Ok(());
        }
        self.end_inner(session)?;
        let active = self.ensure(root, session)?;
        let before = active.store.snapshot()?;
        active.journal.inflight = Some(before.clone());
        // A new turn ends the chance to redo an undone one: the redo would
        // bring back files this turn may since have changed.
        active.journal.redo.clear();
        save_journal(
            &active.store,
            &active.base,
            &active.session,
            &active.journal,
        )?;
        active.pending = Some(Pending {
            prompt: prompt.to_owned(),
            seq_before,
            before,
        });
        Ok(())
    }

    /// The turn has finished: record the tree it left, completing its entry.
    ///
    /// # Errors
    /// When the store cannot be written.
    pub fn end(&mut self, session: &str) -> Result<()> {
        self.end_inner(session)
    }

    fn end_inner(&mut self, session: &str) -> Result<()> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if active.pending.is_none() || active.session != session {
            return Ok(());
        }
        let after = active.store.snapshot()?;
        let Some(pending) = active.pending.take() else {
            return Ok(());
        };
        // A turn that changed no file is still a turn to undo: its conversation
        // goes, and the file move is simply empty.
        active.journal.entries.push(Entry {
            prompt: pending.prompt,
            session: active.session.clone(),
            seq_before: pending.seq_before,
            before: pending.before,
            after,
        });
        active.journal.inflight = None;
        let excess = active.journal.entries.len().saturating_sub(MAX_ENTRIES);
        active.journal.entries.drain(..excess);
        save_journal(
            &active.store,
            &active.base,
            &active.session,
            &active.journal,
        )
    }

    /// Whether a turn has begun and not been settled.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.pending.is_some())
    }

    /// Turns that can be undone in `session`.
    #[must_use]
    pub fn undoable(&mut self, root: &Path, session: &str) -> usize {
        self.ensure(root, session)
            .map_or(0, |a| a.journal.entries.len())
    }

    /// The last turn's undo, planned and not applied.
    ///
    /// # Errors
    /// When nothing is recorded, or the store fails. A turn whose recorded
    /// state has gone from the store is dropped from the journal, so the next
    /// `/undo` reaches the turn before it instead of failing the same way.
    pub fn prepare_undo(&mut self, root: &Path, session: &str) -> Result<Step> {
        let active = self.ensure(root, session)?;
        let Some(entry) = active.journal.entries.last().cloned() else {
            return err("nothing to undo: no turn has been recorded in this session");
        };
        match active.store.plan(&entry.after, &entry.before) {
            Ok(plan) => Ok(Step {
                entry,
                plan,
                session: None,
            }),
            Err(e) => {
                active.journal.entries.pop();
                let _ = save_journal(
                    &active.store,
                    &active.base,
                    &active.session,
                    &active.journal,
                );
                err(format!(
                    "the recorded state of that turn is no longer available ({e}); it was dropped, /undo again goes back one turn further"
                ))
            }
        }
    }

    /// Apply an undo step and record the child session that continues from
    /// it. The caller has already forked the session the turn was recorded in
    /// at `step.entry.seq_before`.
    ///
    /// The child's journal is written first: a failure there leaves every file
    /// as it was.
    ///
    /// # Errors
    /// When the store or a journal cannot be read or written.
    pub fn commit_undo(
        &mut self,
        root: &Path,
        parent: &str,
        child: &str,
        step: &Step,
        force: bool,
    ) -> Result<Applied> {
        let active = self.ensure(root, parent)?;
        let mut entries = active.journal.entries.clone();
        if entries.last() == Some(&step.entry) {
            entries.pop();
        }
        if !session_name_ok(child) {
            return err("the new session id is not usable as a file name");
        }
        save_journal(
            &active.store,
            &active.base,
            child,
            &Journal {
                entries,
                redo: vec![Undone {
                    entry: step.entry.clone(),
                    session: parent.to_owned(),
                }],
                inflight: None,
            },
        )?;
        active.store.apply(&step.plan, force)
    }

    /// The most recently undone turn in `session`, planned and not applied.
    ///
    /// # Errors
    /// When there is nothing to redo, or the store fails.
    pub fn prepare_redo(&mut self, root: &Path, session: &str) -> Result<Step> {
        let active = self.ensure(root, session)?;
        let Some(undone) = active.journal.redo.last().cloned() else {
            return err("nothing to redo");
        };
        let plan = active
            .store
            .plan(&undone.entry.before, &undone.entry.after)?;
        Ok(Step {
            entry: undone.entry,
            plan,
            session: Some(undone.session),
        })
    }

    /// Apply a redo step and drop it from `session`'s redo list. The caller
    /// switches to `step.session`, where the turn still lives.
    ///
    /// # Errors
    /// When the store or the journal cannot be written.
    pub fn commit_redo(
        &mut self,
        root: &Path,
        session: &str,
        step: &Step,
        force: bool,
    ) -> Result<Applied> {
        let active = self.ensure(root, session)?;
        active.journal.redo.pop();
        save_journal(
            &active.store,
            &active.base,
            &active.session,
            &active.journal,
        )?;
        active.store.apply(&step.plan, force)
    }
}

/// Words for a plan: what would change, and what stands in the way.
#[must_use]
pub fn describe_plan(plan: &Plan) -> String {
    const SHOWN: usize = 12;
    let mut lines = Vec::new();
    let pending: Vec<&Item> = plan.pending().filter(|i| i.conflict.is_none()).collect();
    for item in pending.iter().take(SHOWN) {
        lines.push(format!("  {} {}", item.action(), item.path));
    }
    if pending.len() > SHOWN {
        lines.push(format!("  ... and {} more", pending.len() - SHOWN));
    }
    for item in plan.conflicts() {
        let why = item.conflict.as_ref().map_or("", Conflict::describe);
        lines.push(format!("  CONFLICT {} ({why})", item.path));
    }
    for path in &plan.unsupported {
        lines.push(format!("  skipped {path} (a link or nested repository)"));
    }
    if lines.is_empty() {
        lines.push("  no files changed".to_owned());
    }
    lines.join("\n")
}

/// Map from path to action, for tests and summaries.
#[must_use]
pub fn actions(plan: &Plan) -> std::collections::BTreeMap<String, &'static str> {
    plan.pending()
        .map(|i| (i.path.clone(), i.action()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Project {
        root: PathBuf,
        home: PathBuf,
    }

    impl Project {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rapid-undo-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join(".rapidlm")).expect("project");
            // The RapidLM home is a sibling, never inside the project: the
            // store must not be part of the tree it records.
            let home = root.with_file_name(format!(
                "{}-home",
                root.file_name().expect("name").to_string_lossy()
            ));
            let _ = fs::remove_dir_all(&home);
            fs::create_dir_all(&home).expect("home");
            Self { root, home }
        }

        fn put(&self, rel: &str, text: &str) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(path, text).expect("write");
        }

        fn read(&self, rel: &str) -> Option<String> {
            fs::read_to_string(self.root.join(rel)).ok()
        }

        fn remove(&self, rel: &str) {
            fs::remove_file(self.root.join(rel)).expect("remove");
        }

        fn store(&self) -> Store {
            Store::open(&self.root, &store_dir(&self.home, &self.root)).expect("store")
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn enabled_state(p: &Project) -> UndoState {
        let mut state = UndoState::default();
        state.enable(&p.home);
        state
    }

    /// Run one "turn": snapshot, let `change` edit the project, snapshot.
    fn turn(project: &Project, store: &Store, change: impl FnOnce(&Project)) -> (TreeId, TreeId) {
        let before = store.snapshot().expect("before");
        change(project);
        let after = store.snapshot().expect("after");
        (before, after)
    }

    #[test]
    fn undo_restores_modified_added_and_deleted_files() {
        let p = Project::new("basic");
        p.put("keep.txt", "keep");
        p.put("edit.txt", "old");
        p.put("gone.txt", "was here");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("edit.txt", "new");
            p.put("added/deep/new.txt", "fresh");
            p.remove("gone.txt");
        });

        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "{plan:?}");
        let actions = actions(&plan);
        assert_eq!(actions["edit.txt"], "revert");
        assert_eq!(actions["added/deep/new.txt"], "remove");
        assert_eq!(actions["gone.txt"], "restore");
        assert!(!actions.contains_key("keep.txt"));

        let applied = store.apply(&plan, false).expect("apply");
        assert!(applied.failed.is_empty(), "{applied:?}");
        assert_eq!(p.read("edit.txt").as_deref(), Some("old"));
        assert_eq!(p.read("gone.txt").as_deref(), Some("was here"));
        assert_eq!(p.read("added/deep/new.txt"), None);
        assert!(
            !p.root.join("added").exists(),
            "directories the turn made and the undo emptied are removed"
        );
        assert_eq!(p.read("keep.txt").as_deref(), Some("keep"));
    }

    #[test]
    fn redo_is_the_same_move_the_other_way() {
        let p = Project::new("redo");
        p.put("a.txt", "one");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| p.put("a.txt", "two"));
        let undo = store.plan(&after, &before).expect("plan");
        store.apply(&undo, false).expect("undo");
        assert_eq!(p.read("a.txt").as_deref(), Some("one"));
        let redo = store.plan(&before, &after).expect("plan");
        assert!(!redo.has_conflicts());
        store.apply(&redo, false).expect("redo");
        assert_eq!(p.read("a.txt").as_deref(), Some("two"));
    }

    #[test]
    fn a_file_edited_after_the_turn_is_a_conflict_and_is_left_alone() {
        let p = Project::new("conflict");
        p.put("a.txt", "one");
        p.put("b.txt", "bee");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("a.txt", "two");
            p.put("b.txt", "bee two");
        });
        p.put("a.txt", "two, and then a person edited it");

        let plan = store.plan(&after, &before).expect("plan");
        let conflicts: Vec<_> = plan.conflicts().map(|i| i.path.as_str()).collect();
        assert_eq!(conflicts, ["a.txt"]);

        // Without force, the conflicting file is kept and the other still moves.
        let applied = store.apply(&plan, false).expect("apply");
        assert_eq!(applied.kept, ["a.txt"]);
        assert_eq!(
            p.read("a.txt").as_deref(),
            Some("two, and then a person edited it")
        );
        assert_eq!(p.read("b.txt").as_deref(), Some("bee"));

        // Forced, it is overwritten.
        let plan = store.plan(&after, &before).expect("plan");
        let applied = store.apply(&plan, true).expect("apply");
        assert_eq!(applied.written, ["a.txt"]);
        assert_eq!(p.read("a.txt").as_deref(), Some("one"));
    }

    #[test]
    fn each_kind_of_drift_is_named() {
        let p = Project::new("drift");
        p.put("modified.txt", "1");
        p.put("deleted.txt", "1");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("modified.txt", "2");
            p.put("deleted.txt", "2");
            p.put("created.txt", "2");
        });
        // Since the turn: one edited, one deleted, and the file the turn made
        // is replaced by a directory.
        p.put("modified.txt", "3");
        p.remove("deleted.txt");
        p.remove("created.txt");
        fs::create_dir(p.root.join("created.txt")).expect("dir");

        let plan = store.plan(&after, &before).expect("plan");
        let by_path: BTreeMap<_, _> = plan
            .items
            .iter()
            .map(|i| (i.path.as_str(), i.conflict.clone()))
            .collect();
        assert_eq!(by_path["modified.txt"], Some(Conflict::Modified));
        assert_eq!(by_path["deleted.txt"], Some(Conflict::Vanished));
        assert_eq!(by_path["created.txt"], Some(Conflict::NotAFile));
    }

    #[test]
    fn a_file_the_person_already_reverted_is_not_a_conflict() {
        let p = Project::new("already");
        p.put("a.txt", "one");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| p.put("a.txt", "two"));
        p.put("a.txt", "one");
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts());
        assert!(plan.is_empty(), "nothing is left to move: {plan:?}");
    }

    #[test]
    fn ignored_files_and_the_projects_runtime_state_are_not_tracked() {
        let p = Project::new("ignored");
        p.put(".gitignore", "build/\n");
        p.put("build/out.bin", "v1");
        p.put(".rapidlm/sessions.sqlite", "db1");
        p.put(".rapidlm/commands/x.md", "cmd1");
        p.put("src.txt", "s1");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("build/out.bin", "v2");
            p.put(".rapidlm/sessions.sqlite", "db2");
            p.put(".rapidlm/commands/x.md", "cmd2");
            p.put("src.txt", "s2");
        });
        let plan = store.plan(&after, &before).expect("plan");
        let paths: BTreeSet<_> = plan.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            BTreeSet::from([".rapidlm/commands/x.md", "src.txt"]),
            "an ignored path and the runtime database are not undone"
        );
        store.apply(&plan, false).expect("apply");
        assert_eq!(p.read("build/out.bin").as_deref(), Some("v2"));
        assert_eq!(p.read(".rapidlm/sessions.sqlite").as_deref(), Some("db2"));
        assert_eq!(p.read(".rapidlm/commands/x.md").as_deref(), Some("cmd1"));
    }

    #[test]
    fn project_line_ending_and_filter_attributes_do_not_change_what_is_stored() {
        let p = Project::new("attrs");
        p.put(".gitattributes", "*.txt text eol=crlf\n*.dat filter=lfs\n");
        p.put("a.txt", "line one\r\nline two\r\n");
        p.put("b.dat", "payload one");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("a.txt", "changed\n");
            p.put("b.dat", "payload two");
        });
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "{plan:?}");
        store.apply(&plan, false).expect("apply");
        assert_eq!(
            p.read("a.txt").as_deref(),
            Some("line one\r\nline two\r\n"),
            "CRLF bytes come back as they were"
        );
        assert_eq!(p.read("b.dat").as_deref(), Some("payload one"));
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_comes_back_and_links_are_reported_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let p = Project::new("modes");
        p.put("run.sh", "#!/bin/sh\n");
        fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.remove("run.sh");
            std::os::unix::fs::symlink("/etc/hosts", p.root.join("link")).expect("link");
        });
        let plan = store.plan(&after, &before).expect("plan");
        assert_eq!(plan.unsupported, ["link"]);
        store.apply(&plan, false).expect("apply");
        let mode = fs::metadata(p.root.join("run.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable again");
        assert!(
            fs::symlink_metadata(p.root.join("link")).is_ok(),
            "a link is reported and left"
        );
    }

    #[test]
    fn paths_that_escape_the_project_are_refused() {
        assert!(safe_relative("a/b.txt"));
        assert!(!safe_relative("../x"));
        assert!(!safe_relative("/etc/passwd"));
        assert!(!safe_relative("a/../../x"));
        assert!(!safe_relative(".git/config"));
        assert!(!safe_relative(""));
    }

    #[test]
    fn the_tracker_records_turns_and_undoes_the_last_one() {
        let p = Project::new("tracker");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        state
            .begin(&p.root, "sess-1", "first prompt", 10)
            .expect("begin");
        p.put("a.txt", "v1");
        state.end("sess-1").expect("end");
        state
            .begin(&p.root, "sess-1", "second prompt", 20)
            .expect("begin");
        p.put("a.txt", "v2");
        p.put("b.txt", "made");
        state.end("sess-1").expect("end");
        assert_eq!(state.undoable(&p.root, "sess-1"), 2);

        let step = state.prepare_undo(&p.root, "sess-1").expect("prepare");
        assert_eq!(step.entry.prompt, "second prompt");
        assert_eq!(step.entry.seq_before, 20);
        let applied = state
            .commit_undo(&p.root, "sess-1", "sess-2", &step, false)
            .expect("commit");
        assert!(applied.failed.is_empty());
        assert_eq!(p.read("a.txt").as_deref(), Some("v1"));
        assert_eq!(p.read("b.txt"), None);

        // The child continues with the first turn on its stack, and can redo.
        assert_eq!(state.undoable(&p.root, "sess-2"), 1);
        let redo = state.prepare_redo(&p.root, "sess-2").expect("redo");
        assert_eq!(redo.entry.prompt, "second prompt");
        assert_eq!(redo.session.as_deref(), Some("sess-1"));
        state
            .commit_redo(&p.root, "sess-2", &redo, false)
            .expect("redo");
        assert_eq!(p.read("a.txt").as_deref(), Some("v2"));
        assert_eq!(p.read("b.txt").as_deref(), Some("made"));
        assert!(
            state.prepare_redo(&p.root, "sess-2").is_err(),
            "nothing left"
        );
        // The turn still stands in the session it was made in.
        assert_eq!(state.undoable(&p.root, "sess-1"), 2);
    }

    #[test]
    fn undo_twice_walks_back_two_turns_and_redo_twice_brings_them_back_in_order() {
        let p = Project::new("twice");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        for (n, seq) in [(1, 10u64), (2, 20)] {
            state
                .begin(&p.root, "s0", &format!("prompt {n}"), seq)
                .expect("begin");
            p.put("a.txt", &format!("v{n}"));
            state.end("s0").expect("end");
        }
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        state
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");
        let step = state.prepare_undo(&p.root, "s1").expect("prepare");
        assert_eq!(step.entry.prompt, "prompt 1");
        state
            .commit_undo(&p.root, "s1", "s2", &step, false)
            .expect("commit");
        assert_eq!(p.read("a.txt").as_deref(), Some("v0"));
        assert!(
            state.prepare_undo(&p.root, "s2").is_err(),
            "nothing left to undo"
        );

        let redo = state.prepare_redo(&p.root, "s2").expect("redo");
        assert_eq!(
            (redo.entry.prompt.as_str(), redo.session.as_deref()),
            ("prompt 1", Some("s1"))
        );
        state
            .commit_redo(&p.root, "s2", &redo, false)
            .expect("commit");
        assert_eq!(p.read("a.txt").as_deref(), Some("v1"));
        let redo = state.prepare_redo(&p.root, "s1").expect("redo");
        assert_eq!(
            (redo.entry.prompt.as_str(), redo.session.as_deref()),
            ("prompt 2", Some("s0"))
        );
        state
            .commit_redo(&p.root, "s1", &redo, false)
            .expect("commit");
        assert_eq!(p.read("a.txt").as_deref(), Some("v2"));
    }

    #[test]
    fn a_new_turn_ends_the_chance_to_redo() {
        let p = Project::new("redoclear");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        state.begin(&p.root, "s0", "p", 1).expect("begin");
        p.put("a.txt", "v1");
        state.end("s0").expect("end");
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        state
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");

        state.begin(&p.root, "s1", "different", 2).expect("begin");
        p.put("a.txt", "other");
        state.end("s1").expect("end");
        assert!(state.prepare_redo(&p.root, "s1").is_err());
    }

    #[test]
    fn the_journal_survives_a_new_process() {
        let p = Project::new("persist");
        p.put("a.txt", "v0");
        {
            let mut state = enabled_state(&p);
            state.begin(&p.root, "s0", "p", 1).expect("begin");
            p.put("a.txt", "v1");
            state.end("s0").expect("end");
        }
        let mut fresh = enabled_state(&p);
        assert_eq!(fresh.undoable(&p.root, "s0"), 1);
        let step = fresh.prepare_undo(&p.root, "s0").expect("prepare");
        fresh
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");
        assert_eq!(p.read("a.txt").as_deref(), Some("v0"));
    }

    #[test]
    fn the_journal_is_capped() {
        let p = Project::new("cap");
        p.put("a.txt", "0");
        let mut state = enabled_state(&p);
        for n in 0..(MAX_ENTRIES + 5) {
            state
                .begin(&p.root, "s0", &format!("p{n}"), n as u64)
                .expect("begin");
            p.put("a.txt", &n.to_string());
            state.end("s0").expect("end");
        }
        assert_eq!(state.undoable(&p.root, "s0"), MAX_ENTRIES);
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        assert_eq!(step.entry.prompt, format!("p{}", MAX_ENTRIES + 4));
    }

    #[test]
    fn an_unusable_session_id_is_refused_rather_than_used_as_a_path() {
        let p = Project::new("badid");
        let mut state = enabled_state(&p);
        assert!(state.begin(&p.root, "../../escape", "p", 1).is_err());
        assert!(state.begin(&p.root, "", "p", 1).is_err());
    }

    #[test]
    fn pruning_drops_pins_for_trees_no_journal_names() {
        let p = Project::new("prune");
        p.put("a.txt", "v0");
        let store = p.store();
        let orphan = {
            p.put("a.txt", "orphan content");
            store.snapshot().expect("snapshot")
        };
        let list = |store: &Store| {
            String::from_utf8_lossy(
                &store
                    .git(&["for-each-ref", "--format=%(refname)", REF_PREFIX], None)
                    .expect("refs"),
            )
            .into_owned()
        };
        assert!(list(&store).contains(orphan.as_str()));
        store.prune(&BTreeSet::new());
        assert!(!list(&store).contains(orphan.as_str()));
    }

    // --- regressions for the review of the first version ------------------

    #[cfg(unix)]
    #[test]
    fn git_hooks_in_the_store_never_run() {
        use std::os::unix::fs::PermissionsExt;
        let p = Project::new("hooks");
        let store = p.store();
        let marker = p.home.join("hook-ran");
        let hooks = store.git_dir.join("hooks");
        fs::create_dir_all(&hooks).expect("hooks dir");
        for name in ["reference-transaction", "pre-auto-gc", "post-index-change"] {
            let script = hooks.join(name);
            fs::write(
                &script,
                format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            )
            .expect("hook");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p.put("a.txt", "1");
        store.snapshot().expect("snapshot");
        assert!(!marker.exists(), "a hook in the store ran");
    }

    #[test]
    fn the_store_lives_outside_the_project_it_records() {
        let p = Project::new("outside");
        let base = store_dir(&p.home, &p.root);
        assert!(!base.starts_with(&p.root), "{base:?}");
        assert!(base.starts_with(&p.home));
        // Distinct projects get distinct stores.
        let other = Project::new("outside2");
        assert_ne!(store_dir(&p.home, &p.root), store_dir(&p.home, &other.root));
    }

    #[test]
    fn each_entry_remembers_its_session_so_the_second_undo_forks_the_right_one() {
        let p = Project::new("origin");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        for (n, seq) in [(1, 10u64), (2, 40)] {
            state
                .begin(&p.root, "s0", &format!("p{n}"), seq)
                .expect("begin");
            p.put("a.txt", &format!("v{n}"));
            state.end("s0").expect("end");
        }
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        assert_eq!(step.entry.session, "s0");
        state
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");

        // A turn made in the child is recorded against the child.
        state.begin(&p.root, "s1", "p3", 5).expect("begin");
        p.put("a.txt", "v3");
        state.end("s1").expect("end");
        let step = state.prepare_undo(&p.root, "s1").expect("prepare");
        assert_eq!(
            (step.entry.prompt.as_str(), step.entry.session.as_str()),
            ("p3", "s1")
        );
        state
            .commit_undo(&p.root, "s1", "s2", &step, false)
            .expect("commit");

        // Undoing the earlier turn must fork where it happened, not where we are.
        let step = state.prepare_undo(&p.root, "s2").expect("prepare");
        assert_eq!(
            (step.entry.prompt.as_str(), step.entry.session.as_str()),
            ("p1", "s0")
        );
        assert_eq!(step.entry.seq_before, 10);
    }

    #[test]
    fn a_journal_written_before_entries_named_their_session_is_read_as_its_own() {
        let p = Project::new("legacy");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        state.begin(&p.root, "s0", "p", 1).expect("begin");
        p.put("a.txt", "v1");
        state.end("s0").expect("end");
        let base = store_dir(&p.home, &p.root);
        let path = journal_path(&base, "s0");
        let mut doc: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        doc["entries"][0]
            .as_object_mut()
            .expect("entry")
            .remove("session");
        fs::write(&path, serde_json::to_vec(&doc).expect("json")).expect("write");
        let mut fresh = enabled_state(&p);
        assert_eq!(
            fresh
                .prepare_undo(&p.root, "s0")
                .expect("prepare")
                .entry
                .session,
            "s0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_mode_only_change_is_undone_and_a_later_chmod_is_a_conflict() {
        use std::os::unix::fs::PermissionsExt;
        let p = Project::new("modeonly");
        p.put("run.sh", "#!/bin/sh\n");
        let store = p.store();
        let mode = |p: &Project| {
            fs::metadata(p.root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111
        };
        let (before, after) = turn(&p, &store, |p| {
            fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        });
        assert_eq!(mode(&p), 0o111);
        let plan = store.plan(&after, &before).expect("plan");
        assert_eq!(actions(&plan)["run.sh"], "revert", "{plan:?}");
        store.apply(&plan, false).expect("apply");
        assert_eq!(mode(&p), 0, "the executable bit the turn added is gone");

        // Removing the bit by hand is just doing the undo early: not a conflict.
        fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let after = store.snapshot().expect("snapshot");
        fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let early = store.plan(&after, &before).expect("plan");
        assert!((!early.has_conflicts() && early.is_empty()), "{early:?}");

        // When the turn changed the content as well, the same chmod leaves a
        // file that matches neither end state.
        p.put("run.sh", "#!/bin/sh\necho changed\n");
        fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let after2 = store.snapshot().expect("snapshot");
        fs::set_permissions(p.root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let conflicted = store.plan(&after2, &before).expect("plan");
        let item = conflicted
            .items
            .iter()
            .find(|i| i.path == "run.sh")
            .expect("item");
        assert_eq!(item.conflict, Some(Conflict::Modified), "{conflicted:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_file_keeps_its_permissions_through_an_undo() {
        use std::os::unix::fs::PermissionsExt;
        let p = Project::new("private");
        p.put(".env", "TOKEN=old\n");
        fs::set_permissions(p.root.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| p.put(".env", "TOKEN=new\n"));
        let plan = store.plan(&after, &before).expect("plan");
        store.apply(&plan, false).expect("apply");
        assert_eq!(p.read(".env").as_deref(), Some("TOKEN=old\n"));
        let mode = fs::metadata(p.root.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "never widened");
    }

    #[cfg(unix)]
    #[test]
    fn names_git_would_quote_or_split_are_compared_and_restored_correctly() {
        let p = Project::new("oddnames");
        for name in ["\"quoted.txt", "two\nlines.txt", "-dash.txt", "sp ace.txt"] {
            p.put(name, "old");
        }
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            for name in ["\"quoted.txt", "two\nlines.txt", "-dash.txt", "sp ace.txt"] {
                p.put(name, "new");
            }
        });
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "{plan:?}");
        assert_eq!(plan.items.len(), 4);
        // And a change after the turn is still seen, for each of them.
        p.put("\"quoted.txt", "edited later");
        p.put("two\nlines.txt", "edited later");
        let plan = store.plan(&after, &before).expect("plan");
        let conflicted: BTreeSet<&str> = plan.conflicts().map(|i| i.path.as_str()).collect();
        assert_eq!(
            conflicted,
            BTreeSet::from(["\"quoted.txt", "two\nlines.txt"])
        );
        let applied = store.apply(&plan, true).expect("apply");
        assert!(applied.failed.is_empty(), "{applied:?}");
        for name in ["\"quoted.txt", "two\nlines.txt", "-dash.txt", "sp ace.txt"] {
            assert_eq!(p.read(name).as_deref(), Some("old"), "{name}");
        }
    }

    #[test]
    fn removing_a_gitignore_the_project_had_makes_turn_created_files_a_conflict() {
        let p = Project::new("ignorerules");
        p.put(".gitignore", "build/\n");
        p.put("build/out.bin", "compiled");
        p.put("src.txt", "s");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| p.remove(".gitignore"));
        // With the rules gone, build/out.bin is now part of the "after" tree.
        let plan = store.plan(&after, &before).expect("plan");
        let guarded = plan
            .items
            .iter()
            .find(|i| i.path == "build/out.bin")
            .expect("item");
        assert_eq!(
            guarded.conflict,
            Some(Conflict::IgnoreRulesChanged),
            "{plan:?}"
        );
        let applied = store.apply(&plan, false).expect("apply");
        assert_eq!(applied.kept, ["build/out.bin"]);
        assert_eq!(
            p.read("build/out.bin").as_deref(),
            Some("compiled"),
            "not deleted"
        );
        assert_eq!(
            p.read(".gitignore").as_deref(),
            Some("build/\n"),
            "the rules come back"
        );
    }

    #[test]
    fn a_gitignore_the_turn_created_does_not_block_removing_the_files_it_created() {
        let p = Project::new("newignore");
        p.put("keep.txt", "k");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put(".gitignore", "*.log\n");
            p.put("made.txt", "m");
        });
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "{plan:?}");
        store.apply(&plan, false).expect("apply");
        assert_eq!(p.read("made.txt"), None);
    }

    #[test]
    fn a_file_the_turn_replaced_with_a_directory_is_undone_without_force() {
        let p = Project::new("filedir");
        p.put("thing", "I was a file");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.remove("thing");
            p.put("thing/inner.txt", "now a directory");
        });
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "{plan:?}");
        let applied = store.apply(&plan, false).expect("apply");
        assert!(applied.failed.is_empty(), "{applied:?}");
        assert_eq!(p.read("thing").as_deref(), Some("I was a file"));
    }

    #[test]
    fn a_file_saved_between_the_plan_and_the_move_is_a_conflict_not_a_casualty() {
        let p = Project::new("recheck");
        p.put("a.txt", "one");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| p.put("a.txt", "two"));
        let plan = store.plan(&after, &before).expect("plan");
        assert!(!plan.has_conflicts(), "clean at planning time");
        p.put(
            "a.txt",
            "saved by an editor while the conversation was forking",
        );
        let applied = store.apply(&plan, false).expect("apply");
        assert_eq!(applied.kept, ["a.txt"]);
        assert_eq!(
            p.read("a.txt").as_deref(),
            Some("saved by an editor while the conversation was forking")
        );
    }

    #[test]
    fn another_process_pruning_cannot_take_a_running_turns_tree() {
        let p = Project::new("prune-race");
        p.put("a.txt", "v0");
        let mut running = enabled_state(&p);
        running.begin(&p.root, "s0", "long turn", 1).expect("begin");
        p.put("a.txt", "v1");
        // Move the store's index off the "before" tree, as a later snapshot
        // would: git keeps the tree the index describes reachable, which would
        // otherwise hide a missing pin.
        p.store().snapshot().expect("a later snapshot");

        // A second process opens the same project and prunes on first use.
        let mut other = enabled_state(&p);
        let _ = other.undoable(&p.root, "unrelated-session");
        // The grace period alone would hide a missing pin, so collect everything
        // that is not pinned right now.
        p.store()
            .git(&["prune", "--expire=now"], None)
            .expect("prune now");

        running.end("s0").expect("end");
        let step = running
            .prepare_undo(&p.root, "s0")
            .expect("the before tree survived");
        let applied = running
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");
        assert!(applied.failed.is_empty());
        assert_eq!(p.read("a.txt").as_deref(), Some("v0"));
    }

    #[test]
    fn an_entry_whose_recorded_state_is_gone_is_dropped_so_the_next_undo_works() {
        let p = Project::new("gone");
        p.put("a.txt", "v0");
        let mut state = enabled_state(&p);
        state.begin(&p.root, "s0", "good", 1).expect("begin");
        p.put("a.txt", "v1");
        state.end("s0").expect("end");
        // A second entry naming trees that are not in the store.
        let base = store_dir(&p.home, &p.root);
        let path = journal_path(&base, "s0");
        let mut journal: Journal = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut bad = journal.entries[0].clone();
        bad.prompt = "bad".to_owned();
        bad.before = TreeId("1".repeat(40));
        bad.after = TreeId("2".repeat(40));
        journal.entries.push(bad);
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let mut fresh = enabled_state(&p);
        let failure = fresh.prepare_undo(&p.root, "s0").expect_err("unusable");
        assert!(failure.0.contains("dropped"), "{failure}");
        let step = fresh
            .prepare_undo(&p.root, "s0")
            .expect("the next one works");
        assert_eq!(step.entry.prompt, "good");
    }

    #[test]
    fn the_projects_personal_git_excludes_are_honoured() {
        let p = Project::new("personal");
        fs::create_dir_all(p.root.join(".git/info")).expect("dirs");
        fs::write(p.root.join(".git/info/exclude"), "scratch/\n").expect("exclude");
        p.put("scratch/data.csv", "d1");
        p.put("src.txt", "s1");
        let store = p.store();
        let (before, after) = turn(&p, &store, |p| {
            p.put("scratch/data.csv", "d2");
            p.put("src.txt", "s2");
        });
        let plan = store.plan(&after, &before).expect("plan");
        let paths: Vec<&str> = plan.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["src.txt"]);
    }

    #[test]
    fn nothing_is_recorded_until_a_home_is_given() {
        let p = Project::new("inert");
        let mut state = UndoState::default();
        state.begin(&p.root, "s0", "p", 1).expect("a no-op");
        assert!(!state.has_pending());
        assert!(!store_dir(&p.home, &p.root).exists());
    }
}
