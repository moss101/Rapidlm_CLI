//! Non-macOS `shell_exec` sandboxing via the tiered `sandbox` crate.
//!
//! macOS keeps its existing Seatbelt path in `exec_tools.rs` unchanged; this
//! module is the `SandboxManager`-backed path for everywhere else, closing
//! the gap where `sandbox: true` previously just failed outright
//! ("sandbox-exec is unavailable on this platform").
//!
//! Deliberately scoped to the always-available `HostRestrictedBackend` tier
//! only: real process-group isolation plus CPU/memory/pid limits, a genuine
//! improvement over today's status quo on Linux/Windows, but not yet the
//! stronger `Container`/`Gvisor` tiers. Which tier to prefer, and how to
//! degrade gracefully when a stronger one isn't available (Docker/Podman or
//! `runsc` missing), is a real policy decision — see `newtask.md` §1.1 —
//! deliberately deferred rather than guessed here.
//!
//! Runs synchronously, unlike the macOS Seatbelt path (an async background
//! job via `self.jobs.start`, polled through `job_status`/`job_output`).
//! Unifying the two into one execution shape is separate follow-up work,
//! not attempted here — see `newtask.md` §1.1's own note on this split.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CancellationToken,
    CanonicalAction, Capability, CapabilityLease, CanonicalHostPath, LeaseIssuer, PolicyDocument,
    PolicySource, PolicyStack, PrincipalRef, ProcessScope, ResourceDescriptor, evaluate, issue,
    request_approval,
};
use protocol::{RepoPath, SandboxTier, SessionId};
use sandbox::{
    HostRestrictedBackend, MountMode, SandboxError, SandboxExecRequest, SandboxManager,
    SandboxMount, SandboxSpec,
};

/// Fixed mount-point label inside the sandbox's virtual filesystem — never
/// shown to the user, just an anchor name shared by the mount and `cwd`.
const MOUNT_TARGET: &str = "workspace";

/// CPU-time ceiling for one sandboxed `shell_exec` call (Modbit `WRK-017`'s
/// CPU axis). Explicit, not `SandboxSpecBuilder`'s own default
/// (1_000 = 1 CPU-second): that generic crate default is far too tight for
/// a general-purpose shell command and was silently in effect here before —
/// confirmed empirically (a `while` loop counting to 200,000,000 was killed
/// by `SIGXCPU` after ~1 CPU-second with no output and no clear error,
/// `execute_shell` only ever reporting the unhelpful "no exit code
/// (signalled)"). 30 CPU-seconds comfortably covers real scripts/builds
/// without being unbounded.
const SANDBOX_CPU_MILLIS: u32 = 30_000;
/// Memory ceiling for one sandboxed `shell_exec` call. Same reasoning as
/// `SANDBOX_CPU_MILLIS`: an explicit, intentional value for this call site
/// rather than the crate's generic 256 MB default.
const SANDBOX_MEMORY_MB: u32 = 1024;

/// Typed failure. Display never echoes command argv or file paths.
#[derive(Debug)]
pub enum SandboxRunError {
    InvalidRoot,
    /// `argv[0]` isn't an existing file: not absolute-and-present, not
    /// present relative to the workspace root, and not found on `$PATH`.
    ProgramNotFound,
    /// A capability-broker step failed while minting the internal proof-of-
    /// authorization lease `SandboxManager` requires. This is plumbing, not
    /// a second independent gate — the real authorization already happened
    /// via the calling tool's `PermissionLattice` decision before this path
    /// is ever reached, so a failure here means the plumbing is broken, not
    /// that the call was actually unauthorized.
    Capability(String),
    Sandbox(SandboxError),
}

impl fmt::Display for SandboxRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot => f.write_str("workspace root is not a valid sandbox mount source"),
            Self::ProgramNotFound => f.write_str("program not found on PATH or in the workspace"),
            Self::Capability(reason) => write!(f, "sandbox lease setup failed: {reason}"),
            Self::Sandbox(err) => write!(f, "sandbox error: {err}"),
        }
    }
}

impl std::error::Error for SandboxRunError {}

/// Outcome of one sandboxed command. `output` is combined, already-bounded
/// stdout+stderr (see `SandboxExecResult::output`).
#[derive(Debug)]
pub struct SandboxRunOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Terminating signal when `exit_code` is `None` and `timed_out` is
    /// `false` — most commonly `SIGXCPU` (24) from the CPU-time ceiling
    /// (`SANDBOX_CPU_MILLIS`) firing before the wall-clock `timeout` did.
    /// Without this, that case was previously indistinguishable from any
    /// other signal death, reported only as "no exit code (signalled)".
    pub signal: Option<i32>,
    pub output: Vec<u8>,
}

fn cap_err(err: impl fmt::Debug) -> SandboxRunError {
    SandboxRunError::Capability(format!("{err:?}"))
}

fn build_manager() -> SandboxManager {
    let mut manager = SandboxManager::new();
    // Always available (process-group + rlimits, no external dependency);
    // registration can only fail on a duplicate-tier or malformed-capability
    // bug, never on host state, so a fresh single registration is infallible
    // in practice — fail loudly rather than silently if that ever changes.
    manager
        .register(Box::new(HostRestrictedBackend::new()))
        .expect("HostRestrictedBackend is the only registered backend and always valid");
    manager
}

fn build_spec(
    root: &Path,
    timeout: Duration,
    output_limit: u64,
) -> Result<SandboxSpec, SandboxRunError> {
    let host_str = root.to_str().ok_or(SandboxRunError::InvalidRoot)?;
    let host =
        CanonicalHostPath::from_resolved(host_str).map_err(|_| SandboxRunError::InvalidRoot)?;
    let target = RepoPath::parse(MOUNT_TARGET).map_err(|_| SandboxRunError::InvalidRoot)?;
    let mount = SandboxMount::bind(host, target.clone(), MountMode::ReadWrite)
        .map_err(SandboxRunError::Sandbox)?;
    SandboxSpec::builder(SandboxTier::HostRestricted)
        .cwd(target)
        .mount(mount)
        .timeout(timeout)
        .output_limit(output_limit)
        .cpu_millis(SANDBOX_CPU_MILLIS)
        .memory_mb(SANDBOX_MEMORY_MB)
        .build()
        .map_err(SandboxRunError::Sandbox)
}

/// Resolve `program` to an absolute, existing file path the way the shell
/// would: absolute paths pass through, a path containing `/` is resolved
/// relative to the workspace `root`, and a bare name is searched for on
/// `$PATH`. Needed because `HostRestrictedBackend` deliberately refuses to
/// run a relative/unresolved program (see its `relative_executable_is_rejected`
/// test) — unlike `std::process::Command`, it does no implicit PATH search.
pub(crate) fn resolve_program(root: &Path, program: &str) -> Result<String, SandboxRunError> {
    let candidate = if program.starts_with('/') {
        Path::new(program).to_path_buf()
    } else if program.contains('/') {
        root.join(program)
    } else {
        return resolve_program_on_path(program);
    };
    std::fs::canonicalize(&candidate)
        .ok()
        .filter(|p| p.is_file())
        .and_then(|p| p.to_str().map(str::to_owned))
        .ok_or(SandboxRunError::ProgramNotFound)
}

fn resolve_program_on_path(program: &str) -> Result<String, SandboxRunError> {
    let path_var = std::env::var_os("PATH").ok_or(SandboxRunError::ProgramNotFound)?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if let Ok(canon) = std::fs::canonicalize(&candidate)
            && canon.is_file()
            && let Some(s) = canon.to_str()
        {
            return Ok(s.to_owned());
        }
    }
    Err(SandboxRunError::ProgramNotFound)
}

/// Mint a single-use `Capability::ProcExec` lease for `command_name`. The
/// policy here is deliberately trivial (a fixed always-`ask` rule, resolved
/// by immediately self-approving) — see `SandboxRunError::Capability`'s doc
/// comment for why that's the correct shape, not a shortcut.
fn mint_proc_exec_lease(
    issuer: &LeaseIssuer,
    command_name: &str,
) -> Result<CapabilityLease, SandboxRunError> {
    let cancel = CancellationToken::new();

    let policy_src = format!(
        "[[rules]]\nid = \"ask\"\neffect = \"ask\"\nsubjects = [\"*\"]\ncapability = \"{}\"\n",
        Capability::ProcExec.as_str()
    );
    let source = PolicySource::user("rapidlm-sandbox-shell-exec").map_err(cap_err)?;
    let doc = PolicyDocument::parse_toml(&policy_src, source, &cancel).map_err(cap_err)?;
    let policies = PolicyStack::new([doc]).map_err(cap_err)?;

    let resource = ResourceDescriptor::Process(ProcessScope::new(command_name).map_err(cap_err)?);
    let actual = CanonicalAction::Resource {
        capability: Capability::ProcExec,
        resource: resource.clone(),
    };
    let principal = PrincipalRef::parse("agent").map_err(cap_err)?;
    let request = ActionRequest::new(
        principal,
        SessionId::new(),
        Capability::ProcExec,
        resource,
        actual,
        "shell_exec sandbox",
    )
    .map_err(cap_err)?;

    let now = Instant::now();
    let decision = evaluate(&policies, &request, &cancel).map_err(cap_err)?;
    let approval = request_approval(&request, &decision, now, &cancel).map_err(cap_err)?;
    let approved = match approval
        .resolve(
            ApprovalChoice::Approve(ApprovalScopeId::Once),
            &request,
            now,
            &cancel,
        )
        .map_err(cap_err)?
    {
        ApprovalResolution::Approved(approved) => approved,
        // Unreachable in practice: the fixed policy above always resolves
        // to Ask, and Approve always approves an Ask decision. Kept as a
        // typed error rather than an `unreachable!()` so a future policy
        // change here fails closed instead of panicking.
        ApprovalResolution::Denied => {
            return Err(SandboxRunError::Capability(
                "internal self-approval was denied".to_owned(),
            ));
        }
    };
    issue(issuer, &approved, &policies, now, &cancel).map_err(cap_err)
}

/// Run `argv` inside the host-restricted sandbox tier, synchronously.
/// `root` must be an existing, canonicalizable directory — the workspace
/// root is mounted read-write and the command's cwd is set to it.
pub fn run_sandboxed(
    root: &Path,
    argv: &[String],
    timeout: Duration,
    output_limit: u64,
) -> Result<SandboxRunOutcome, SandboxRunError> {
    let manager = build_manager();
    let spec = build_spec(root, timeout, output_limit)?;
    let issuer = LeaseIssuer::ephemeral();
    let command_name = argv.first().map(String::as_str).unwrap_or("shell");
    let lease = mint_proc_exec_lease(&issuer, command_name)?;
    let cancel = CancellationToken::new();

    let handle = manager
        .prepare(&spec, &lease, &cancel)
        .map_err(SandboxRunError::Sandbox)?;
    let program = resolve_program(root, command_name)?;
    let resolved_argv = std::iter::once(program).chain(argv.iter().skip(1).cloned());
    let request = SandboxExecRequest::new(resolved_argv, timeout, output_limit)
        .map_err(SandboxRunError::Sandbox)?;
    let result = manager.exec(&spec, &handle, &request, &lease, &cancel);
    // Best-effort cleanup: a destroy failure after a successful/failed exec
    // is a resource-leak concern for the backend's own bookkeeping, not
    // something the caller can act on — never mask the exec outcome with it.
    let _ = manager.destroy(&handle, &cancel);
    let result = result.map_err(SandboxRunError::Sandbox)?;

    Ok(SandboxRunOutcome {
        exit_code: result.exit().code(),
        timed_out: result.exit().timed_out(),
        signal: result.exit().signal(),
        output: result.output().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-sandbox-exec-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        std::fs::canonicalize(&root).expect("canonicalize")
    }

    #[test]
    fn run_sandboxed_executes_and_captures_real_output() {
        let root = temp_root("basic");
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "echo hello-from-sandbox".to_owned()];
        let outcome = run_sandboxed(&root, &argv, Duration::from_secs(5), 4096).expect("run");
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        let output = String::from_utf8_lossy(&outcome.output);
        assert!(output.contains("hello-from-sandbox"), "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_sandboxed_runs_with_cwd_at_the_mounted_workspace_root() {
        let root = temp_root("cwd");
        std::fs::write(root.join("marker.txt"), b"present").expect("write marker");
        let argv = vec!["ls".to_owned()];
        let outcome = run_sandboxed(&root, &argv, Duration::from_secs(5), 4096).expect("run");
        assert_eq!(outcome.exit_code, Some(0));
        let output = String::from_utf8_lossy(&outcome.output);
        assert!(
            output.contains("marker.txt"),
            "the sandboxed process should see the mounted workspace root's own files: {output}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_sandboxed_reports_a_nonzero_exit_without_erroring() {
        let root = temp_root("nonzero");
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()];
        let outcome = run_sandboxed(&root, &argv, Duration::from_secs(5), 4096).expect("run");
        assert_eq!(outcome.exit_code, Some(3));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_sandboxed_times_out_a_long_running_command() {
        let root = temp_root("timeout");
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()];
        let outcome =
            run_sandboxed(&root, &argv, Duration::from_millis(300), 4096).expect("run");
        assert!(outcome.timed_out);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_sandboxed_survives_a_moderately_cpu_heavy_command() {
        // Regression: SandboxSpec::builder's own generic default
        // (cpu_millis: 1_000, i.e. 1 CPU-second) was silently in effect here
        // before SANDBOX_CPU_MILLIS existed, and a real shell loop like this
        // one (confirmed empirically, ~2.4 CPU-seconds) was killed by
        // SIGXCPU well under any wall-clock timeout, with no output and no
        // informative error — a legitimate command, not a runaway one.
        let root = temp_root("cpu-heavy");
        // Pure shell-builtin arithmetic (`[`/`$(())`), no subprocess per
        // iteration: a loop that shells out to `date` on every pass barely
        // touches this *process's own* CPU time no matter how long it runs
        // wall-clock-wise (RLIMIT_CPU only counts the process it's set on,
        // not descendants it forks and waits on). This iteration count is
        // measured directly on real hardware (`time /bin/sh -c '...'`) at
        // ~4 real/CPU seconds — comfortably past the *old* 1-CPU-second
        // default (the actual bug) and comfortably under the new 30-second
        // one, with margin for slower CI hardware in both directions.
        let argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "i=0; while [ $i -lt 1500000 ]; do i=$((i+1)); done; echo done-looping".to_owned(),
        ];
        let outcome = run_sandboxed(&root, &argv, Duration::from_secs(30), 4096).expect("run");
        assert_eq!(outcome.exit_code, Some(0), "signal={:?}", outcome.signal);
        assert!(!outcome.timed_out);
        let output = String::from_utf8_lossy(&outcome.output);
        assert!(output.contains("done-looping"), "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_sandboxed_rejects_a_nonexistent_root() {
        let root = std::env::temp_dir().join("rapidlm-sandbox-exec-does-not-exist");
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()];
        let err = run_sandboxed(&root, &argv, Duration::from_secs(5), 4096)
            .expect_err("nonexistent root must fail, not silently run somewhere else");
        // Whatever the exact failure mode (mount rejects a non-canonical or
        // absent path), it must be a typed error, never a panic.
        let _ = err;
    }
}
