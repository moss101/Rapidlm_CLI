//! `rapid permissions list|allow|revoke`: the explicit, human-only writer for
//! the persisted per-project grant store.
//!
//! [`crate::permissions::PermissionLattice::evaluate`]'s step 3 —
//! "persisted per-project grants suppress the ask" — has been implemented and
//! tested since it was written, and was **unreachable in production**:
//! `parse_grants` was a reader with no counterpart, so
//! `Decision::PersistedGrant` could never be returned by a real run. Nothing
//! anywhere created a grant.
//!
//! That matters more than it sounds, because `PermissionMode::Default` — the
//! out-of-box mode for the interactive TUI *and* headless exec — resolves
//! every non-read call to `Ask`, and no surface in this build can prompt for
//! an approval, so `ExecTools` denies it. Until the approval path exists (see
//! `newtask.md`'s "OPEN DECISION — the interactive TUI cannot ask for
//! approval"), a persisted grant is the only mechanism that lets a user
//! pre-approve a specific tool for a specific project *without* widening the
//! permission mode for everything.
//!
//! Boundaries:
//!
//! * Writing a grant is a privilege escalation, so this is reachable only
//!   from this process's argv — no model tool, slash command, hook, or
//!   autonomous-goal path dispatches a subcommand. Exactly the argument
//!   `run_trust_command` rests on, and the reason `/permissions` in the TUI
//!   points here rather than writing grants itself.
//! * A grant can only ever *narrow* the gap between `Ask` and `Allow`. It
//!   cannot widen past a managed-policy tool ban, or past a write-scope
//!   ceiling for the file-edit tools that ceiling governs: `evaluate` checks
//!   both at steps -1 and -0.5, before rules and grants. (A write-scope
//!   ceiling classifies only file edits, so it never constrained
//!   `shell_exec`, with or without a grant.)
//! * The store is never wider than owner-only (`0o600`) — it names exactly
//!   which tools run without being asked. A narrower mode the user chose is
//!   kept; a wider one is tightened on the next write.
//! * The whole read-modify-write is held under a sibling `.lock`, so two
//!   concurrent runs cannot lose each other's grants.
//! * Grants are keyed by canonical project root, resolved through the same
//!   `resolve_project_root` every trust check and `rapid mcp` use, so a grant
//!   made here is observed by the next run in the same project.

use std::fs::File;
use std::path::{Path, PathBuf};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::interactive::{
    PERMISSIONS_STORE_NAME, TRUST_CATALOG_NAME, resolve_project_root, user_home_from,
};
use crate::permissions::{GrantsError, PermissionGrants, ToolPattern, parse_grants, render_grants};

/// Process inputs, injectable so the command is testable without changing the
/// test process's working directory or environment — the same shape
/// [`crate::mcp_admin::McpEnv`] and [`crate::doctor::DoctorEnv`] use.
pub struct PermissionsEnv {
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Overrides the home resolved from `env`, for a caller that already has
    /// one. Typed rather than a `String` env pair: `Path::display()` mangles
    /// non-UTF-8 paths and the resolver *creates* what it is handed.
    pub home: Option<PathBuf>,
}

impl PermissionsEnv {
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: std::env::vars().collect(),
            home: None,
        }
    }
}

/// Text for stdout plus the process exit code, returned rather than printed
/// so every behavior below is assertable without capturing a stream.
#[derive(Debug, PartialEq, Eq)]
pub struct PermissionsOutcome {
    pub text: String,
    pub exit: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PermissionsUsageError(pub String);

pub const PERMISSIONS_USAGE: &str = "\
usage: rapid permissions list|allow|revoke

Pre-approve specific tool calls for this project, persisted per project root
in the RapidLM home. A grant is consulted by every run in this project and
suppresses the approval this build cannot yet prompt for.

Commands:
  list                    Every grant recorded for this project.
  allow <pattern>...      Record grants. Idempotent: re-granting an existing
                          pattern reports so and changes nothing.
  revoke <pattern>...     Remove grants.

  -h, --help              Print this help

Pattern syntax is `Tool` or `Tool(arg-glob)`, the same grammar the
`permissions` rules in .rapidlm/settings.json use — for example
`workspace_write` or `shell_exec(git *)`.

A pattern may name a tool this build does not provide; it is recorded as
intent and simply never matches. `rapid tools` prints the real names.

A grant can only narrow the gap between \"ask\" and \"allow\". It cannot widen
past a managed-policy tool ban, or past a write-scope ceiling for the
file-edit tools that ceiling governs — both are checked before grants are
consulted. Note that a write-scope ceiling classifies only file edits, so it
never constrained `shell_exec` in the first place, with or without a grant.
A grant also does nothing in a project that is not trusted, where every tool
call is refused outright.

Exit code:
  0   the command succeeded
  1   the store could not be read or written, or a revoke matched nothing
  2   usage error
";

/// Dispatch one `rapid permissions` invocation.
pub fn run(
    args: &[String],
    env: &PermissionsEnv,
) -> Result<PermissionsOutcome, PermissionsUsageError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(PermissionsOutcome {
            text: PERMISSIONS_USAGE.to_owned(),
            exit: 0,
        });
    }
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(PermissionsUsageError(
            "rapid permissions: no command given".to_owned(),
        ));
    };
    let rest = &args[1..];
    // Resolution failures are *environment* failures, not usage errors: an
    // unresolvable home or working directory exited 2 and printed the whole
    // pattern-syntax help, which is neither the documented contract nor
    // useful. The documented `1` ("the store could not be read or written")
    // is what they are.
    let project = match resolve(env) {
        Ok(project) => project,
        Err(reason) => {
            return Ok(PermissionsOutcome {
                text: format!("error: {reason}\n"),
                exit: 1,
            });
        }
    };
    match sub {
        "list" => {
            if let Some(unexpected) = rest.first() {
                return Err(PermissionsUsageError(format!(
                    "rapid permissions list: unexpected argument '{unexpected}'"
                )));
            }
            Ok(list(&project))
        }
        "allow" => mutate(&project, rest, Mutation::Allow),
        "revoke" => mutate(&project, rest, Mutation::Revoke),
        other => Err(PermissionsUsageError(format!(
            "rapid permissions: unknown command '{other}'"
        ))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mutation {
    Allow,
    Revoke,
}

impl Mutation {
    fn name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Revoke => "revoke",
        }
    }
}

/// The project and store this command operates on.
struct Project {
    /// Canonical root — the exact string the grant store is keyed by, and
    /// the same one `exec_permission_lattice` looks up at run time.
    canonical_root: String,
    display_root: PathBuf,
    store_path: PathBuf,
    trust: Result<TrustStatus, String>,
}

fn resolve(env: &PermissionsEnv) -> Result<Project, String> {
    let cancel = CancellationToken::new();
    let found = resolve_project_root(&env.cwd, &cancel)?;
    let home = match env.home.clone() {
        Some(home) => home,
        None => user_home_from(&env.env)
            .ok_or_else(|| "no RapidLM home directory could be resolved".to_owned())?,
    };
    // `persisted_grants_for` looks grants up under `fs::canonicalize(root)`,
    // so the key written here must be produced the same way or a grant would
    // be recorded under a path no run ever queries.
    //
    // `resolve_project_root` already canonicalizes (it starts with
    // `canonicalize_dir`), so this is belt-and-braces rather than a fix for
    // a reachable bug — which is why the test below proves the *end-to-end*
    // agreement through a symlinked working directory rather than asserting
    // this line in isolation, where it cannot fail.
    let canonical = std::fs::canonicalize(&found.root)
        .map_err(|err| format!("{} could not be canonicalized: {err}", found.root.display()))?;
    let trust = trust_of(&canonical, &home, &cancel);
    Ok(Project {
        canonical_root: canonical.to_string_lossy().into_owned(),
        display_root: canonical,
        store_path: home.join(PERMISSIONS_STORE_NAME),
        trust,
    })
}

/// Record one persisted grant programmatically — the "approve and remember"
/// path of the pending-approval flow (`approvals.rs`). Same store, lock,
/// bounds and owner-only rules as the `rapid permissions allow` writer; the
/// only difference is the caller (the session loop, on an explicit
/// approve-and-remember decision) and the pattern's provenance.
pub(crate) fn record_persisted_grant(
    project_root: &Path,
    user_home: &Path,
    pattern: &str,
) -> Result<bool, String> {
    let parsed =
        ToolPattern::parse(pattern).ok_or_else(|| format!("invalid grant pattern: {pattern}"))?;
    let canonical = std::fs::canonicalize(project_root).map_err(|err| {
        format!(
            "{} could not be canonicalized: {err}",
            project_root.display()
        )
    })?;
    let project = Project {
        canonical_root: canonical.to_string_lossy().into_owned(),
        display_root: canonical,
        store_path: user_home.join(PERMISSIONS_STORE_NAME),
        // Unused by load/save; recorded for construction completeness only.
        trust: Ok(TrustStatus::Trusted),
    };
    let _lock = GrantsLock::acquire(&project.store_path)?;
    let mut grants = load(&project)?;
    let changed = grants
        .allow(&project.canonical_root, parsed)
        .map_err(|err| format!("the grant store rejected the pattern: {err:?}"))?;
    if changed {
        save(&project, &grants, None)?;
    }
    Ok(changed)
}

fn trust_of(root: &Path, home: &Path, cancel: &CancellationToken) -> Result<TrustStatus, String> {
    let identity = ProjectIdentity::new(root, None)
        .map_err(|err| format!("project identity could not be derived: {err}"))?;
    ProjectTrustStore::open(home.join(TRUST_CATALOG_NAME))
        .get(&identity, cancel)
        .map_err(|err| format!("trust state could not be read: {err}"))
}

/// Advisory cross-process lock over the grant store, held across the whole
/// read-modify-write.
///
/// Without it the store is a plain load-mutate-save and concurrent runs lose
/// each other's records: 24 concurrent `rapid permissions allow`, one per
/// project, all reported success and left 2 records; a `revoke` racing an
/// `allow` reported `revoke=` and exited 0 with the grant still present —
/// telling a user a dangerous grant is gone when it is not.
///
/// Deliberately the same shape as `kernel::project::trust`'s `TrustLock` and
/// `GoalHost`'s `GoalLock`: a sibling `.lock` file whose content is never
/// read, created if absent, held for the lifetime of the guard.
struct GrantsLock {
    _file: File,
}

impl GrantsLock {
    fn acquire(store: &Path) -> Result<Self, String> {
        let mut lock_path = store.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("{} could not be created: {err}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| format!("{} could not be opened: {err}", lock_path.display()))?;
        file.lock()
            .map_err(|err| format!("{} could not be locked: {err}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

/// Read the store, or the empty set when it does not exist yet.
///
/// A store that exists but does not parse is an error, never an overwrite:
/// the run-time reader treats an unparsable store as "no grants" (fail
/// closed, correct there), but a *writer* that silently replaced it would
/// destroy every other project's grants to record one.
fn load(project: &Project) -> Result<PermissionGrants, String> {
    match std::fs::read_to_string(&project.store_path) {
        Ok(text) if text.trim().is_empty() => Ok(PermissionGrants::default()),
        Ok(text) => parse_grants(&text).map_err(|err| {
            format!(
                "{} is not a usable grant store ({}); fix or remove it by hand rather than \
letting this command overwrite every project's grants",
                project.store_path.display(),
                describe(err)
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(PermissionGrants::default()),
        Err(err) => Err(format!(
            "{} could not be read: {err}",
            project.store_path.display()
        )),
    }
}

fn save(project: &Project, grants: &PermissionGrants, mode: Option<u32>) -> Result<(), String> {
    let text = render_grants(grants)
        .map_err(|err| format!("the grant store could not be encoded ({})", describe(err)))?;
    if let Some(parent) = project.store_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("{} could not be created: {err}", parent.display()))?;
    }
    let mut bytes = text.into_bytes();
    bytes.push(b'\n');
    // Bounded on what is actually written, not on `render_grants`'s
    // pre-newline text: a document rendering to exactly MAX_SETTINGS_BYTES
    // was written as one byte more and became permanently unreadable, at
    // which point `persisted_grants_for` fails closed and *every* project's
    // grants vanish with no message.
    if bytes.len() > crate::permissions::MAX_SETTINGS_BYTES {
        return Err(format!(
            "the grant store would exceed the {}-byte limit its own loader enforces; revoke some grants first",
            crate::permissions::MAX_SETTINGS_BYTES
        ));
    }
    // Owner-only: the store names the exact tools a project may run without
    // being asked, so another local account must not be able to add one.
    crate::exec_tools::atomic_write_with_mode(&project.store_path, &bytes, mode).map_err(
        |err| {
            format!(
                "{} could not be written: {err}",
                project.store_path.display()
            )
        },
    )?;
    enforce_store_mode(project, mode);
    Ok(())
}

/// Apply [`store_mode`] to the store after the rename.
///
/// `OpenOptions::mode()` is masked by the process umask, so the mode set at
/// creation time is only an upper bound — under `umask 077` a preserved
/// `0o640` silently became `0o600`. Since the target here is *at most*
/// `0o600`, a umask can only ever make the file narrower than intended,
/// which is safe; this call makes the resulting mode exact and independent
/// of the umask so the guarantee is a fact rather than a coincidence.
#[cfg(unix)]
fn enforce_store_mode(project: &Project, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        let _ =
            std::fs::set_permissions(&project.store_path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn enforce_store_mode(_project: &Project, _mode: Option<u32>) {}

/// The mode the store must end up with: at most `0o600`.
///
/// Never wider than owner-only, and never widened by what is already there.
/// An existing store may be *narrower* (a user who chose `0o400` keeps it),
/// but a pre-existing `0o666` — or a symlink pointing at one — must not
/// survive: this file enumerates exactly which tools run without being
/// asked. `symlink_metadata`, not `metadata`, so a symlink's own mode is
/// read rather than its target's.
#[cfg(unix)]
fn store_mode(project: &Project) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    const OWNER_ONLY: u32 = 0o600;
    match std::fs::symlink_metadata(&project.store_path) {
        Ok(metadata) => Some(metadata.permissions().mode() & 0o7777 & OWNER_ONLY),
        Err(_) => Some(OWNER_ONLY),
    }
}

#[cfg(not(unix))]
fn store_mode(_project: &Project) -> Option<u32> {
    None
}

fn describe(err: GrantsError) -> &'static str {
    match err {
        GrantsError::TooLarge => "the document is over the size limit",
        GrantsError::InvalidJson => "it is not the expected JSON shape",
        GrantsError::InvalidGrant => "it contains an unparseable pattern, or too many grants",
        GrantsError::TooManyRecords => "it records too many projects",
    }
}

fn header(project: &Project) -> String {
    format!(
        "project={} trust={}\nstore={}\n",
        project.display_root.display(),
        match &project.trust {
            Ok(status) => status.as_str().to_owned(),
            Err(reason) => format!("unreadable ({reason})"),
        },
        project.store_path.display(),
    )
}

fn list(project: &Project) -> PermissionsOutcome {
    let mut text = header(project);
    let grants = match load(project) {
        Ok(grants) => grants,
        Err(reason) => {
            text.push_str(&format!("error: {reason}\n"));
            return PermissionsOutcome { text, exit: 1 };
        }
    };
    let allowed = grants.for_root(&project.canonical_root);
    text.push_str(&format!("grants={}\n", allowed.len()));
    for pattern in &allowed {
        text.push_str(&format!("allow={}\n", pattern.render()));
    }
    if allowed.is_empty() {
        text.push_str(
            "note: no grant is recorded for this project; every non-read tool call is \
decided by the permission mode alone\n",
        );
    }
    text.push_str(&untrusted_note(project));
    PermissionsOutcome { text, exit: 0 }
}

fn mutate(
    project: &Project,
    args: &[String],
    mutation: Mutation,
) -> Result<PermissionsOutcome, PermissionsUsageError> {
    if args.is_empty() {
        return Err(PermissionsUsageError(format!(
            "rapid permissions {}: no pattern given",
            mutation.name()
        )));
    }
    let mut patterns = Vec::with_capacity(args.len());
    for raw in args {
        if raw.starts_with('-') {
            return Err(PermissionsUsageError(format!(
                "rapid permissions {}: unexpected option '{raw}'",
                mutation.name()
            )));
        }
        // Parsed with the loader's own grammar, so this command can never
        // write a pattern the reader would reject.
        let pattern = ToolPattern::parse(raw).ok_or_else(|| {
            PermissionsUsageError(format!(
                "rapid permissions {}: {raw:?} is not a valid pattern (expected `Tool` or \
`Tool(arg-glob)`)",
                mutation.name()
            ))
        })?;
        // A pattern naming a tool this build does not provide is accepted
        // rather than rejected — a grant for a tool added later, or one
        // since removed, still reads as intent, the same reason
        // `ToolPattern::parse` accepts unknown names for deny rules. It is
        // *not* warned about: there is no single list of advertised tool
        // names in this crate (the set lives in three separate `match`
        // arms), and introducing a fourth to check against would be a list
        // that drifts. `PERMISSIONS_USAGE` says so and points at
        // `rapid tools`, which prints the real surface.
        patterns.push(pattern);
    }

    let mut text = header(project);
    // Held across load -> mutate -> save: without it two concurrent runs
    // lose each other's records, and a `revoke` can report success while the
    // grant it removed is written back by a racing `allow`.
    let _lock = match GrantsLock::acquire(&project.store_path) {
        Ok(lock) => lock,
        Err(reason) => {
            text.push_str(&format!("error: {reason}\n"));
            return Ok(PermissionsOutcome { text, exit: 1 });
        }
    };
    // Sampled before the write: `atomic_write_with_mode` renames a fresh
    // file into place, so afterwards there is nothing left to read the
    // previous mode from.
    let mode = store_mode(project);
    let mut grants = match load(project) {
        Ok(grants) => grants,
        Err(reason) => {
            text.push_str(&format!("error: {reason}\n"));
            return Ok(PermissionsOutcome { text, exit: 1 });
        }
    };
    // Accumulated, not printed, until the store write succeeds: a run that
    // failed to persist used to print `allow=<tool>` and *then* the error,
    // reporting grants that do not exist — and the early return on a
    // `GrantsError` left those lines standing for changes that were dropped.
    let mut lines: Vec<String> = Vec::with_capacity(patterns.len());
    let mut changed = 0usize;
    let mut unchanged = 0usize;
    for pattern in patterns {
        let rendered = pattern.render();
        let outcome = match mutation {
            Mutation::Allow => match grants.allow(&project.canonical_root, pattern) {
                Ok(changed) => changed,
                Err(err) => {
                    text.push_str(&format!(
                        "error: {rendered} not granted: {}\n",
                        describe(err)
                    ));
                    return Ok(PermissionsOutcome { text, exit: 1 });
                }
            },
            Mutation::Revoke => grants.revoke(&project.canonical_root, &pattern),
        };
        if outcome {
            changed += 1;
            lines.push(format!("{}={rendered}\n", mutation.name()));
        } else {
            unchanged += 1;
            lines.push(match mutation {
                Mutation::Allow => format!("already-granted={rendered}\n"),
                Mutation::Revoke => format!("not-granted={rendered}\n"),
            });
        }
    }
    if changed > 0
        && let Err(reason) = save(project, &grants, mode)
    {
        text.push_str(&format!("error: {reason}\n"));
        return Ok(PermissionsOutcome { text, exit: 1 });
    }
    for line in lines {
        text.push_str(&line);
    }
    // Revoking nothing at all is a failed request, not a silent success: the
    // user named a pattern that was not there.
    let exit = if mutation == Mutation::Revoke && changed == 0 && unchanged > 0 {
        1
    } else {
        0
    };
    if mutation == Mutation::Allow && changed > 0 {
        text.push_str(&untrusted_note(project));
    }
    Ok(PermissionsOutcome { text, exit })
}

/// A grant in an untrusted project records intent but changes nothing: every
/// tool call is refused before the lattice is consulted at all.
fn untrusted_note(project: &Project) -> String {
    match &project.trust {
        Ok(TrustStatus::Trusted) => String::new(),
        _ => "note: this project is not trusted, so every tool call is refused regardless of \
any grant; run `rapid trust grant` here first\n"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        project: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rapidlm-permcli-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&root);
            let project = root.join("project");
            let home = root.join("home");
            std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
            std::fs::create_dir_all(&home).expect("home");
            Self {
                root,
                project,
                home,
            }
        }

        fn env(&self) -> PermissionsEnv {
            PermissionsEnv {
                cwd: self.project.clone(),
                env: Vec::new(),
                home: Some(self.home.clone()),
            }
        }

        fn run(&self, args: &[&str]) -> PermissionsOutcome {
            run(
                &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(),
                &self.env(),
            )
            .expect("not a usage error")
        }

        fn usage(&self, args: &[&str]) -> PermissionsUsageError {
            run(
                &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(),
                &self.env(),
            )
            .expect_err("expected a usage error")
        }

        fn store(&self) -> PathBuf {
            self.home.join(PERMISSIONS_STORE_NAME)
        }

        fn trust(&self) {
            let canonical = std::fs::canonicalize(&self.project).expect("canonicalize");
            let identity = ProjectIdentity::new(&canonical, None).expect("identity");
            ProjectTrustStore::open(self.home.join(TRUST_CATALOG_NAME))
                .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
                .expect("grant trust");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// **The test this whole command exists for.** A grant must actually
    /// change what a real run decides — otherwise it is a file nobody reads.
    ///
    /// Drives the exact chain `build_interactive_turn_context` builds:
    /// `persisted_grants_for` (the production reader, split out of
    /// `exec_permission_lattice` only so a test can supply the home) then
    /// `ExecTools::workspace_with_permissions`. Before the grant this is
    /// `interactive::tests::default_mode_denies_every_write_because_nothing_
    /// can_prompt_for_approval`; after it, the same call succeeds.
    #[test]
    fn a_granted_tool_actually_becomes_allowed_in_the_default_mode() {
        use crate::permissions::{PermissionLattice, PermissionMode};
        use agent_runtime::ToolDriver;

        let fixture = Fixture::new("effective");
        let canonical = std::fs::canonicalize(&fixture.project).expect("canonicalize");

        let lattice = |fixture: &Fixture| {
            PermissionLattice::new(PermissionMode::Default).with_grants(
                crate::interactive::persisted_grants_for(&canonical, &fixture.home),
            )
        };
        let write_call = || {
            agent_runtime::ProposedToolCall::new(
                "c1",
                crate::exec_tools::WORKSPACE_WRITE_TOOL,
                r#"{"path":"note.md","content":"hello"}"#,
            )
            .expect("call")
        };
        let cancel = agent_runtime::CancellationToken::new();

        // Before: default mode asks, and nothing can prompt, so it is denied.
        let mut tools =
            crate::exec_tools::ExecTools::workspace_with_permissions(&canonical, lattice(&fixture))
                .expect("tools");
        let validated = tools.validate(&write_call(), &cancel).expect("validate");
        assert!(
            matches!(
                tools.execute(&validated, &cancel).expect("execute"),
                agent_runtime::ToolStepResult::Denied { .. }
            ),
            "precondition: an ungranted write is denied in default mode"
        );

        // Grant it through the real command.
        let outcome = fixture.run(&["allow", crate::exec_tools::WORKSPACE_WRITE_TOOL]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);

        // After: the same production chain now allows the same call.
        let mut tools =
            crate::exec_tools::ExecTools::workspace_with_permissions(&canonical, lattice(&fixture))
                .expect("tools");
        let validated = tools.validate(&write_call(), &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            agent_runtime::ToolStepResult::Succeeded { .. } => {}
            other => panic!("a persisted grant did not reach the running lattice: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(canonical.join("note.md")).expect("written"),
            "hello",
            "the granted call must really run, not just be classified as allowed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_grant_made_through_a_symlinked_path_is_still_found_by_a_real_run() {
        // The store is keyed by canonical root and `persisted_grants_for`
        // looks it up with `fs::canonicalize(root)`. A grant recorded under
        // any other spelling of the same directory would be invisible at run
        // time while this command still reported success.
        //
        // Driven through a symlink because that is the only way to make the
        // two spellings actually differ: asserting the canonicalization in
        // `resolve` directly cannot fail, since `resolve_project_root`
        // already canonicalizes before this code sees the path.
        let fixture = Fixture::new("symlink");
        let link = fixture.root.join("link-to-project");
        std::os::unix::fs::symlink(&fixture.project, &link).expect("symlink");
        assert_ne!(
            link,
            std::fs::canonicalize(&link).expect("canonicalize"),
            "precondition: the symlinked path differs from the real one"
        );

        let env = PermissionsEnv {
            cwd: link,
            env: Vec::new(),
            home: Some(fixture.home.clone()),
        };
        let outcome = run(&["allow".to_owned(), "workspace_write".to_owned()], &env)
            .expect("not a usage error");
        assert_eq!(outcome.exit, 0, "{}", outcome.text);

        // Looked up the way a real run does, from the real directory.
        let canonical = std::fs::canonicalize(&fixture.project).expect("canonicalize");
        let grants = crate::interactive::persisted_grants_for(&canonical, &fixture.home);
        assert_eq!(
            grants.iter().map(ToolPattern::render).collect::<Vec<_>>(),
            vec!["workspace_write".to_owned()],
            "a grant made through a symlinked path was recorded where no run will look"
        );
    }

    #[test]
    fn granting_is_idempotent_and_revoking_what_was_never_granted_fails() {
        let fixture = Fixture::new("idem");
        assert!(
            fixture
                .run(&["allow", "workspace_write"])
                .text
                .contains("allow=workspace_write")
        );
        let again = fixture.run(&["allow", "workspace_write"]);
        assert_eq!(again.exit, 0);
        assert!(
            again.text.contains("already-granted=workspace_write"),
            "{}",
            again.text
        );
        let revoke = fixture.run(&["revoke", "workspace_write"]);
        assert_eq!(revoke.exit, 0, "{}", revoke.text);
        let missing = fixture.run(&["revoke", "workspace_write"]);
        assert_eq!(
            missing.exit, 1,
            "revoking something that was not granted is a failed request, not a silent success"
        );
    }

    #[test]
    fn a_store_this_command_cannot_parse_is_refused_never_overwritten() {
        // The run-time reader treats an unparsable store as "no grants",
        // which is correct fail-closed behavior there. A *writer* with the
        // same leniency would destroy every other project's grants in order
        // to record one.
        let fixture = Fixture::new("corrupt");
        let before = r#"{ "schema": 1, "projects": [ THIS IS NOT JSON"#;
        std::fs::write(fixture.store(), before).expect("write store");
        let outcome = fixture.run(&["allow", "workspace_write"]);
        assert_eq!(outcome.exit, 1);
        assert!(
            outcome.text.contains("not a usable grant store"),
            "{}",
            outcome.text
        );
        assert_eq!(
            std::fs::read_to_string(fixture.store()).expect("still there"),
            before,
            "the unparsable store must be left exactly as it was"
        );
    }

    #[test]
    fn another_projects_grants_survive_a_write() {
        let fixture = Fixture::new("multi");
        std::fs::write(
            fixture.store(),
            r#"{"schema":1,"projects":[{"root":"/somewhere/else","allow":["shell_exec"]}]}"#,
        )
        .expect("seed store");
        assert_eq!(fixture.run(&["allow", "workspace_write"]).exit, 0);
        let text = std::fs::read_to_string(fixture.store()).expect("store");
        let grants = crate::permissions::parse_grants(&text).expect("parses");
        assert_eq!(
            grants
                .for_root("/somewhere/else")
                .iter()
                .map(ToolPattern::render)
                .collect::<Vec<_>>(),
            vec!["shell_exec".to_owned()],
            "another project's grants were lost:\n{text}"
        );
    }

    #[test]
    fn an_untrusted_project_is_told_the_grant_changes_nothing_yet() {
        // A grant in an untrusted project is real and recorded, but every
        // tool call is refused before the lattice is consulted at all.
        let fixture = Fixture::new("untrusted");
        let outcome = fixture.run(&["allow", "workspace_write"]);
        assert_eq!(outcome.exit, 0);
        assert!(
            outcome.text.contains("rapid trust grant"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.contains("trust=untrusted"), "{}", outcome.text);

        fixture.trust();
        let outcome = fixture.run(&["list"]);
        assert!(outcome.text.contains("trust=trusted"), "{}", outcome.text);
        assert!(
            !outcome.text.contains("rapid trust grant"),
            "a trusted project needs no such note: {}",
            outcome.text
        );
    }

    #[test]
    fn a_pattern_the_loader_would_reject_is_never_written() {
        let fixture = Fixture::new("badpattern");
        let err = fixture.usage(&["allow", "not a pattern"]);
        assert!(err.0.contains("not a valid pattern"), "{}", err.0);
        assert!(
            !fixture.store().exists(),
            "a rejected pattern must not create a store"
        );
    }

    #[test]
    fn malformed_invocations_are_usage_errors() {
        let fixture = Fixture::new("usage");
        for args in [
            vec!["bogus"],
            vec!["list", "extra"],
            vec!["allow"],
            vec!["revoke"],
            vec!["allow", "--flag"],
        ] {
            let _ = fixture.usage(&args);
        }
    }

    #[test]
    fn help_is_available_and_exits_zero() {
        let fixture = Fixture::new("help");
        let outcome = fixture.run(&["--help"]);
        assert_eq!(outcome.exit, 0);
        assert_eq!(outcome.text, PERMISSIONS_USAGE);
    }

    #[test]
    #[cfg(unix)]
    fn the_store_is_never_wider_than_owner_only_whatever_it_was_before() {
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        }

        let fixture = Fixture::new("mode");
        assert_eq!(fixture.run(&["allow", "workspace_write"]).exit, 0);
        assert_eq!(
            mode_of(&fixture.store()),
            0o600,
            "the store names exactly what may run unasked"
        );

        // A store someone widened must be tightened again, not preserved:
        // an earlier version read the existing mode and carried it forward,
        // so a 0o666 store stayed world-readable through every later write.
        std::fs::set_permissions(fixture.store(), std::fs::Permissions::from_mode(0o666))
            .expect("widen");
        assert_eq!(fixture.run(&["allow", "repo_read"]).exit, 0);
        assert_eq!(
            mode_of(&fixture.store()),
            0o600,
            "a widened store was not tightened"
        );

        // A *narrower* choice is the user's to keep.
        std::fs::set_permissions(fixture.store(), std::fs::Permissions::from_mode(0o400))
            .expect("narrow");
        assert_eq!(fixture.run(&["allow", "repo_glob"]).exit, 0);
        assert_eq!(
            mode_of(&fixture.store()),
            0o400,
            "a narrower mode the user chose was widened"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_final_mode_does_not_depend_on_the_process_umask() {
        // `OpenOptions::mode()` is masked by the umask, so the mode set when
        // the temp file is created is only an upper bound. The mode is
        // therefore applied again after the rename; without that, this
        // suite's result changed with the umask of whoever ran it.
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("umask");
        assert_eq!(fixture.run(&["allow", "workspace_write"]).exit, 0);
        let mode = std::fs::metadata(fixture.store())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert!(
            mode == 0o600 || mode & 0o077 == 0,
            "the store must never be group- or world-accessible (0o{mode:o})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_write_reports_no_grant_it_did_not_persist() {
        // The per-pattern `allow=` lines used to be printed inside the loop,
        // before the store write, so a failed save produced
        // `allow=workspace_write` followed by the error — reporting a grant
        // that does not exist.
        //
        // The failure has to happen at *save*, not at *load*: a first
        // attempt at this test put a directory where the store belongs,
        // which makes `load` fail and return before the loop is ever
        // reached, so it passed with the bug reintroduced. A read-only home
        // lets the load succeed (an absent store reads as no grants) and
        // fails only when the temp file is created.
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("failedwrite");
        // One successful run first, so the store *and* the lock file already
        // exist: opening an existing file for writing needs permission on
        // the file, not the directory, so the lock still acquires and the
        // load still succeeds once the home is read-only. Without this the
        // run fails at `GrantsLock::acquire`, before the loop, and the test
        // passes with the bug reintroduced.
        assert_eq!(fixture.run(&["allow", "repo_read"]).exit, 0);
        std::fs::set_permissions(&fixture.home, std::fs::Permissions::from_mode(0o500))
            .expect("read-only home");
        let outcome = fixture.run(&["allow", "workspace_write"]);
        // Restore before any assertion so the fixture can always clean up.
        std::fs::set_permissions(&fixture.home, std::fs::Permissions::from_mode(0o700))
            .expect("restore");

        assert_eq!(outcome.exit, 1, "{}", outcome.text);
        assert!(
            !outcome.text.contains("allow=workspace_write"),
            "reported a grant it could not persist:\n{}",
            outcome.text
        );
        assert!(outcome.text.contains("error:"), "{}", outcome.text);
    }

    #[test]
    fn a_store_that_would_exceed_the_loader_s_own_limit_is_refused() {
        // `render_grants` bounds its text, then `save` appends a newline —
        // so a document rendering to exactly the limit was written one byte
        // over and became permanently unreadable, at which point the
        // run-time reader fails closed and *every* project's grants vanish.
        let fixture = Fixture::new("toobig");
        // Fill the store to just under the limit with other projects.
        let mut grants = crate::permissions::PermissionGrants::default();
        let filler = ToolPattern::parse("workspace_write").expect("pattern");
        let mut index = 0usize;
        loop {
            let mut candidate = grants.clone();
            if candidate
                .allow(&format!("/pad/{index:06}"), filler.clone())
                .is_err()
            {
                break;
            }
            match crate::permissions::render_grants(&candidate) {
                Ok(text) if text.len() < crate::permissions::MAX_SETTINGS_BYTES - 64 => {
                    grants = candidate;
                    index += 1;
                }
                _ => break,
            }
        }
        std::fs::write(
            fixture.store(),
            crate::permissions::render_grants(&grants).expect("render"),
        )
        .expect("seed");

        // Now add grants for this project until the writer refuses.
        let mut refused = false;
        for step in 0..64 {
            let outcome = fixture.run(&["allow", &format!("tool_{step:04}")]);
            if outcome.exit == 1 {
                assert!(outcome.text.contains("limit"), "{}", outcome.text);
                refused = true;
                break;
            }
        }
        assert!(refused, "the writer never refused an over-limit store");
        // Whatever it refused to write, the store must still be readable.
        let text = std::fs::read_to_string(fixture.store()).expect("store");
        assert!(
            text.len() <= crate::permissions::MAX_SETTINGS_BYTES,
            "the store on disk is over the loader's limit ({} bytes)",
            text.len()
        );
        crate::permissions::parse_grants(&text)
            .expect("the store this command left behind must still parse");
    }

    #[test]
    fn concurrent_writers_do_not_lose_each_other_s_grants() {
        // Without a lock this is a plain read-modify-write: 24 concurrent
        // runs, one per project, all reported success and left 2 records.
        let fixture = Fixture::new("concurrent");
        let count = 12usize;
        let home = fixture.home.clone();
        let handles: Vec<_> = (0..count)
            .map(|index| {
                let root = fixture.root.join(format!("p{index}"));
                std::fs::create_dir_all(root.join(".rapidlm")).expect("project");
                let home = home.clone();
                std::thread::spawn(move || {
                    let env = PermissionsEnv {
                        cwd: root,
                        env: Vec::new(),
                        home: Some(home),
                    };
                    run(&["allow".to_owned(), "workspace_write".to_owned()], &env)
                        .expect("not a usage error")
                        .exit
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().expect("thread"), 0);
        }
        let text = std::fs::read_to_string(fixture.store()).expect("store");
        let grants = crate::permissions::parse_grants(&text).expect("parses");
        for index in 0..count {
            let root = std::fs::canonicalize(fixture.root.join(format!("p{index}")))
                .expect("canonicalize");
            assert_eq!(
                grants
                    .for_root(&root.to_string_lossy())
                    .iter()
                    .map(ToolPattern::render)
                    .collect::<Vec<_>>(),
                vec!["workspace_write".to_owned()],
                "a concurrent run's grant was lost:\n{text}"
            );
        }
    }
}
