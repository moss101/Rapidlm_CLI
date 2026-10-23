//! Turn-level undo: take the files a turn changed back to how they were, and
//! take the conversation back to just before it.
//!
//! # How the files are tracked
//!
//! A private bare git repository under `<project>/.rapidlm/undo/` records the
//! project's working tree at the start and the end of every turn. Only tree
//! objects are written, never commits, and the repository's work tree is the
//! project itself, so what is recorded is what `git add -A` would see: the
//! project's own ignore rules apply, build output and dependencies are left
//! out, and a change made by a shell command is captured exactly like one made
//! by a file tool. Content filters and line-ending conversion are switched off
//! for this repository, so what is stored is the bytes on disk.
//!
//! # What undo promises
//!
//! Restoring a turn means moving each file the turn changed from its end state
//! back to its start state. A file is moved only if it is **still as the turn
//! left it**. A file edited since (by the person, or by a later tool) is a
//! conflict, and a conflict stops the whole undo unless it is forced, so work
//! done after the turn is never overwritten by accident. Redo is the same move
//! in the other direction, with the same check.
//!
//! Symbolic links and nested repositories are not restorable and are reported.
//! Files the project ignores are not tracked, so an ignored file the agent
//! edited stays as it is.
//!
//! The module knows nothing about sessions' event ledgers; the interactive loop
//! does the conversation half (forking the session back to the sequence this
//! module recorded) and calls in here for the files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Subdirectory of the project marker that holds the store and journals.
pub const DIR: &str = "undo";

/// Environment variable that turns tracking off (`RAPIDLM_NO_UNDO=1`). A very
/// large working tree makes two `git add -A` passes per turn a real cost.
pub const DISABLE_ENV: &str = "RAPIDLM_NO_UNDO";

/// Most turns remembered per session.
pub const MAX_ENTRIES: usize = 50;

/// How long an idle session's journal is kept before pruning.
const JOURNAL_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Ref namespace pinning every recorded tree, so git never collects one that
/// a journal still names.
const REF_PREFIX: &str = "refs/rapid/";

/// Largest file `redo`/`undo` will read back from the store into memory.
const MAX_RESTORE_BYTES: usize = 256 * 1024 * 1024;

/// Whether tracking is switched off by the environment.
#[must_use]
pub fn disabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
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
}

/// Why a file cannot be moved without losing something.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Conflict {
    /// The file's content differs from what the turn left.
    Modified,
    /// The turn left no file here, and one exists now.
    Appeared,
    /// The turn left a file here, and it is gone now.
    Vanished,
    /// A directory, link or other non-file sits at the path.
    NotAFile,
}

impl Conflict {
    fn describe(&self) -> &'static str {
        match self {
            Self::Modified => "changed since",
            Self::Appeared => "created since",
            Self::Vanished => "deleted since",
            Self::NotAFile => "is not a regular file now",
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
];

/// The store's own ignore rules. The project's `.rapidlm/` holds runtime state
/// (the session database, locks, this store); only its hand-written parts are
/// work an agent might change and a person might want back.
const EXCLUDES: &str = "/.rapidlm/*\n!/.rapidlm/commands/\n!/.rapidlm/MEMORY.md\n";

/// Attributes that switch off every content transformation, so a stored blob is
/// the file's bytes. They outrank the project's own `.gitattributes`.
const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding -eol\n";

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
            let output = Command::new("git")
                .arg("init")
                .arg("--bare")
                .arg("-q")
                .arg(&store.git_dir)
                .env_remove("GIT_DIR")
                .stdin(Stdio::null())
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
        write_if_different(&info.join("exclude"), EXCLUDES)?;
        write_if_different(&info.join("attributes"), ATTRIBUTES)?;
        Ok(store)
    }

    fn git(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>> {
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
            ])
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
        // at the first unreadable file.
        self.git(&["add", "-A", "--ignore-errors"], None)
            .or_else(|e| {
                // An unreadable file makes `add` exit non-zero after staging the
                // rest; the tree written next is still what was readable.
                if e.0.contains("unable to") || e.0.contains("Permission denied") {
                    Ok(Vec::new())
                } else {
                    Err(e)
                }
            })?;
        let out = self.git(&["write-tree"], None)?;
        let id = String::from_utf8_lossy(&out).trim().to_owned();
        if !is_oid(&id) {
            return err("git did not return a tree id");
        }
        // Pin it. A ref may point at any object, and an unpinned tree is
        // unreachable and so eligible for collection.
        self.git(&["update-ref", &format!("{REF_PREFIX}{id}"), &id], None)?;
        Ok(TreeId(id))
    }

    /// The moves that take a project now matching `expected` to `target`.
    ///
    /// Nothing is changed. Each regular file is checked against the working
    /// tree: it must be as `expected` says (or already be what `target`
    /// says), or it is recorded as a conflict.
    ///
    /// # Errors
    /// When git fails.
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
            });
        }
        self.classify(&mut plan.items)?;
        Ok(plan)
    }

    /// Compare each item's file on disk with what it is expected to be.
    fn classify(&self, items: &mut [Item]) -> Result<()> {
        let mut to_hash = Vec::new();
        for (index, item) in items.iter_mut().enumerate() {
            let on_disk = self.root.join(&item.path);
            match fs::symlink_metadata(&on_disk) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    match (&item.expected, &item.target) {
                        (None, _) => {}
                        (Some(_), None) => item.already = true,
                        (Some(_), Some(_)) => item.conflict = Some(Conflict::Vanished),
                    }
                }
                Err(e) => return Err(e.into()),
                Ok(meta) if meta.is_file() => match (&item.expected, &item.target) {
                    (None, None) => {}
                    (None, Some(_)) => item.conflict = Some(Conflict::Appeared),
                    (Some(_), _) => to_hash.push(index),
                },
                Ok(_) => item.conflict = Some(Conflict::NotAFile),
            }
        }
        if to_hash.is_empty() {
            return Ok(());
        }
        let list: Vec<u8> = to_hash
            .iter()
            .flat_map(|&i| {
                let mut line = items[i].path.clone().into_bytes();
                line.push(b'\n');
                line
            })
            .collect();
        let out = self.git(
            &["hash-object", "--no-filters", "--stdin-paths"],
            Some(list),
        )?;
        let hashes: Vec<&str> = std::str::from_utf8(&out)
            .map_err(|_| UndoError("git returned non-text hashes".to_owned()))?
            .lines()
            .collect();
        if hashes.len() != to_hash.len() {
            return err("git hashed a different number of files than asked");
        }
        for (&index, hash) in to_hash.iter().zip(hashes) {
            let item = &mut items[index];
            let expected = item.expected.as_ref().map(|b| b.oid.as_str());
            let target = item.target.as_ref().map(|b| b.oid.as_str());
            if Some(hash) == target {
                item.already = true;
            } else if Some(hash) != expected {
                item.conflict = Some(Conflict::Modified);
            }
        }
        Ok(())
    }

    /// Carry out `plan`. Conflicting files are left alone unless `force`.
    ///
    /// # Errors
    /// Only when the store itself cannot be read; a file that cannot be moved
    /// is reported in [`Applied::failed`] and the rest are still moved.
    pub fn apply(&self, plan: &Plan, force: bool) -> Result<Applied> {
        let mut applied = Applied::default();
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
            if item.conflict.is_some() && !force {
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
        if fs::symlink_metadata(destination).is_ok_and(|m| m.is_dir()) {
            return err("a directory is in the way");
        }
        let name = destination
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging = destination.with_file_name(format!(".{name}.undo-{}", std::process::id()));
        fs::write(&staging, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if blob.mode == 0o100_755 { 0o755 } else { 0o644 };
            fs::set_permissions(&staging, fs::Permissions::from_mode(mode))?;
        }
        if let Err(e) = fs::rename(&staging, destination) {
            let _ = fs::remove_file(&staging);
            return Err(e.into());
        }
        Ok(())
    }

    /// Drop pins for trees no journal names, and collect them.
    fn prune(&self, keep: &BTreeSet<String>) {
        let Ok(refs) = self.git(&["for-each-ref", "--format=%(refname)", REF_PREFIX], None) else {
            return;
        };
        let mut dropped = false;
        for name in String::from_utf8_lossy(&refs).lines() {
            let id = name.trim_start_matches(REF_PREFIX);
            if !keep.contains(id) && self.git(&["update-ref", "-d", name], None).is_ok() {
                dropped = true;
            }
        }
        if dropped {
            let _ = self.git(&["gc", "--prune=now", "--quiet"], None);
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
    fs::read(journal_path(base, session))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_journal(base: &Path, session: &str, journal: &Journal) -> io::Result<()> {
    let path = journal_path(base, session);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let staging = path.with_extension("json.tmp");
    fs::write(&staging, serde_json::to_vec(journal)?)?;
    fs::rename(&staging, &path)
}

/// Every tree any journal names, and journals past their age limit removed.
fn live_trees_after_expiry(base: &Path) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    let Ok(entries) = fs::read_dir(base.join("journal")) else {
        return live;
    };
    for file in entries.filter_map(std::result::Result::ok) {
        let path = file.path();
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
        for entry in journal
            .entries
            .iter()
            .chain(journal.redo.iter().map(|u| &u.entry))
        {
            live.insert(entry.before.0.clone());
            live.insert(entry.after.0.clone());
        }
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
    enabled: bool,
    warned: bool,
}

impl UndoState {
    /// Start recording turns. A state that was never enabled records nothing
    /// and touches no disk, which keeps one-shot helpers and tests inert.
    pub fn enable(&mut self) {
        self.enabled = true;
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
        let base = root.join(crate::interactive::PROJECT_MARKER).join(DIR);
        let reload = self
            .active
            .as_ref()
            .is_none_or(|a| a.session != session || a.base != base);
        if reload {
            // Leave a pending snapshot of the previous session unrecorded; it
            // was never settled, so there is no end state to pair it with.
            let store = match self.active.take() {
                Some(a) if a.base == base => a.store,
                _ => Store::open(root, &base)?,
            };
            if !self.pruned {
                self.pruned = true;
                let live = live_trees_after_expiry(&base);
                store.prune(&live);
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
        if !self.enabled || disabled() {
            return Ok(());
        }
        self.end_inner(root, session)?;
        let active = self.ensure(root, session)?;
        let before = active.store.snapshot()?;
        active.pending = Some(Pending {
            prompt: prompt.to_owned(),
            seq_before,
            before,
        });
        // A new turn ends the chance to redo an undone one: the redo would
        // bring back files this turn may since have changed.
        if !active.journal.redo.is_empty() {
            active.journal.redo.clear();
            save_journal(&active.base, &active.session, &active.journal)?;
        }
        Ok(())
    }

    /// The turn has finished: record the tree it left, completing its entry.
    ///
    /// # Errors
    /// When the store cannot be written.
    pub fn end(&mut self, root: &Path, session: &str) -> Result<()> {
        self.end_inner(root, session)
    }

    fn end_inner(&mut self, root: &Path, session: &str) -> Result<()> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if active.pending.is_none() || active.session != session {
            return Ok(());
        }
        let _ = root;
        let after = active.store.snapshot()?;
        let Some(pending) = active.pending.take() else {
            return Ok(());
        };
        // A turn that changed no file is still a turn to undo: its conversation
        // goes, and the file move is simply empty.
        active.journal.entries.push(Entry {
            prompt: pending.prompt,
            seq_before: pending.seq_before,
            before: pending.before,
            after,
        });
        let excess = active.journal.entries.len().saturating_sub(MAX_ENTRIES);
        active.journal.entries.drain(..excess);
        save_journal(&active.base, &active.session, &active.journal)?;
        Ok(())
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
    /// When nothing is recorded, or the store fails.
    pub fn prepare_undo(&mut self, root: &Path, session: &str) -> Result<Step> {
        let active = self.ensure(root, session)?;
        let Some(entry) = active.journal.entries.last().cloned() else {
            return err("nothing to undo: no turn has been recorded in this session");
        };
        let plan = active.store.plan(&entry.after, &entry.before)?;
        Ok(Step {
            entry,
            plan,
            session: None,
        })
    }

    /// Apply an undo step and record the child session that continues from
    /// it. The caller has already forked the session at `step.entry.seq_before`.
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
        let applied = active.store.apply(&step.plan, force)?;
        let mut entries = active.journal.entries.clone();
        if entries.last() == Some(&step.entry) {
            entries.pop();
        }
        let base = active.base.clone();
        if session_name_ok(child) {
            save_journal(
                &base,
                child,
                &Journal {
                    entries,
                    redo: vec![Undone {
                        entry: step.entry.clone(),
                        session: parent.to_owned(),
                    }],
                },
            )?;
        }
        Ok(applied)
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
        let applied = active.store.apply(&step.plan, force)?;
        active.journal.redo.pop();
        save_journal(&active.base, &active.session, &active.journal)?;
        Ok(applied)
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
pub fn actions(plan: &Plan) -> BTreeMap<String, &'static str> {
    plan.pending()
        .map(|i| (i.path.clone(), i.action()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Project {
        root: PathBuf,
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
            Self { root }
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
            Store::open(&self.root, &self.root.join(".rapidlm").join(DIR)).expect("store")
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn enabled_state() -> UndoState {
        let mut state = UndoState::default();
        state.enable();
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
        let mut state = enabled_state();
        state
            .begin(&p.root, "sess-1", "first prompt", 10)
            .expect("begin");
        p.put("a.txt", "v1");
        state.end(&p.root, "sess-1").expect("end");
        state
            .begin(&p.root, "sess-1", "second prompt", 20)
            .expect("begin");
        p.put("a.txt", "v2");
        p.put("b.txt", "made");
        state.end(&p.root, "sess-1").expect("end");
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
        let mut state = enabled_state();
        for (n, seq) in [(1, 10u64), (2, 20)] {
            state
                .begin(&p.root, "s0", &format!("prompt {n}"), seq)
                .expect("begin");
            p.put("a.txt", &format!("v{n}"));
            state.end(&p.root, "s0").expect("end");
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
        let mut state = enabled_state();
        state.begin(&p.root, "s0", "p", 1).expect("begin");
        p.put("a.txt", "v1");
        state.end(&p.root, "s0").expect("end");
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        state
            .commit_undo(&p.root, "s0", "s1", &step, false)
            .expect("commit");

        state.begin(&p.root, "s1", "different", 2).expect("begin");
        p.put("a.txt", "other");
        state.end(&p.root, "s1").expect("end");
        assert!(state.prepare_redo(&p.root, "s1").is_err());
    }

    #[test]
    fn the_journal_survives_a_new_process() {
        let p = Project::new("persist");
        p.put("a.txt", "v0");
        {
            let mut state = enabled_state();
            state.begin(&p.root, "s0", "p", 1).expect("begin");
            p.put("a.txt", "v1");
            state.end(&p.root, "s0").expect("end");
        }
        let mut fresh = enabled_state();
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
        let mut state = enabled_state();
        for n in 0..(MAX_ENTRIES + 5) {
            state
                .begin(&p.root, "s0", &format!("p{n}"), n as u64)
                .expect("begin");
            p.put("a.txt", &n.to_string());
            state.end(&p.root, "s0").expect("end");
        }
        assert_eq!(state.undoable(&p.root, "s0"), MAX_ENTRIES);
        let step = state.prepare_undo(&p.root, "s0").expect("prepare");
        assert_eq!(step.entry.prompt, format!("p{}", MAX_ENTRIES + 4));
    }

    #[test]
    fn an_unusable_session_id_is_refused_rather_than_used_as_a_path() {
        let p = Project::new("badid");
        let mut state = enabled_state();
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
}
