//! `shell_exec` sandboxing via the tiered `sandbox` crate, shared by both
//! live platform paths.
//!
//! This module builds the `SandboxManager`/`SandboxSpec` plumbing both paths
//! need and exposes two entry points that differ only in which backend they
//! register and how they're driven: [`build_manager`] +
//! synchronous [`run_sandboxed`] (everywhere except macOS with `sandbox-exec`
//! present — `HostRestrictedBackend`, real process-group isolation plus
//! CPU/memory/pid limits, closing the gap where `sandbox: true` previously
//! just failed outright there), and [`build_manager_seatbelt`] (macOS,
//! `SeatbeltBackend`, driven asynchronously from
//! `exec_tools.rs::JobRegistry::start_sandboxed` as a background job polled
//! through `job_status`/`job_output` — the same async shape that path
//! already had before it gained real resource governance). The two entry
//! points aren't merged into one execution shape because the macOS job path
//! needs to keep its async, pollable UX; see `newtask.md` §1.1 for why that
//! was the deciding constraint.
//!
//! Deliberately scoped to the `HostRestrictedBackend`/`SeatbeltBackend`
//! tier only: not yet the stronger `Container`/`Gvisor` tiers. Which tier to
//! prefer, and how to degrade gracefully when a stronger one isn't available
//! (Docker/Podman or `runsc` missing), is a real policy decision — see
//! `newtask.md` §1.1 — deliberately deferred rather than guessed here.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CancellationToken,
    CanonicalAction, CanonicalHostPath, Capability, CapabilityLease, LeaseIssuer, LeaseValidator,
    PolicyDocument, PolicyRevision, PolicySource, PolicyStack, PrincipalRef, ProcessScope,
    ResourceDescriptor, evaluate, issue, request_approval,
};
use protocol::{RepoPath, SandboxTier, SessionId};
use sandbox::{
    HostRestrictedBackend, MountMode, SandboxError, SandboxExecRequest, SandboxManager,
    SandboxMount, SandboxNetwork, SandboxSpec, SeatbeltBackend,
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
    /// The sandbox's own memory ceiling killed the process (`SandboxExit::
    /// oom`). Reported with no `signal` at all by the backend, so without
    /// this flag an OOM kill was indistinguishable from any other silent
    /// signal death too — the same gap the CPU/`signal` field closed for
    /// `SIGXCPU`, just for the axis that doesn't surface as a signal number.
    pub oom: bool,
    /// The sandbox's process-count ceiling was exceeded (`SandboxExit::
    /// policy_violation`) — same "reported with no signal" shape as `oom`.
    pub policy_violation: bool,
    pub output: Vec<u8>,
}

fn cap_err(err: impl fmt::Debug) -> SandboxRunError {
    SandboxRunError::Capability(format!("{err:?}"))
}

pub(crate) fn build_manager() -> SandboxManager {
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

/// Same shape as [`build_manager`], but for the macOS async sandboxed-job
/// path (`exec_tools.rs::JobRegistry::start_sandboxed`): registers
/// [`SeatbeltBackend`] instead of [`HostRestrictedBackend`] at the same
/// [`SandboxTier::HostRestricted`] tier (the two are mutually exclusive at
/// one tier per `SandboxManager` — never call this and [`build_manager`] on
/// the same `SandboxManager`). Registration is infallible for the same
/// reason `build_manager`'s own doc comment gives.
pub(crate) fn build_manager_seatbelt() -> SandboxManager {
    let mut manager = SandboxManager::new();
    manager
        .register(Box::new(SeatbeltBackend::new()))
        .expect("SeatbeltBackend is the only registered backend and always valid");
    manager
}

/// `network` is a real parameter, not always `SandboxNetwork::None`, because
/// this is now shared by two callers with different, deliberate network
/// postures: `run_sandboxed` (below) keeps requesting `None` — unchanged,
/// `HostRestrictedBackend` doesn't enforce it anyway (a separate, pre-
/// existing, not-fixed-here gap) — while `JobRegistry::start_sandboxed`
/// requests `Open`, preserving the network-open behavior the macOS async
/// job path already has today rather than silently tightening it as a side
/// effect of adding resource governance.
pub(crate) fn build_spec(
    root: &Path,
    timeout: Duration,
    output_limit: u64,
    network: SandboxNetwork,
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
        .network(network)
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
    protocol::host_path::canonicalize(&candidate)
        .ok()
        .filter(|p| p.is_file())
        .and_then(|p| p.to_str().map(str::to_owned))
        .ok_or(SandboxRunError::ProgramNotFound)
}

fn resolve_program_on_path(program: &str) -> Result<String, SandboxRunError> {
    let path_var = std::env::var_os("PATH").ok_or(SandboxRunError::ProgramNotFound)?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if let Ok(canon) = protocol::host_path::canonicalize(&candidate)
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
///
/// Returns the `PolicyRevision` of the policy stack the lease was minted
/// against alongside the lease itself: the caller needs it to construct a
/// `LeaseValidator` that actually agrees with what's baked into the lease
/// (`LeaseValidator::new`'s own revision check fails closed on a mismatch),
/// rather than recomputing the policy document a second time and hoping it
/// stays byte-for-byte identical.
///
/// `pub(crate)` alongside `build_manager`: reused as-is by
/// `external_scan.rs`'s `SupervisedScannerExec` impl, which needs the exact
/// same lease-minting ceremony for a plan `ExternalScannerAdapter::plan`
/// already built, rather than a second copy of this policy/approval dance.
pub(crate) fn mint_proc_exec_lease(
    issuer: &LeaseIssuer,
    command_name: &str,
) -> Result<(CapabilityLease, PolicyRevision), SandboxRunError> {
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
    let revision = PolicyRevision::of_stack(&policies);
    let lease = issue(issuer, &approved, &policies, now, &cancel).map_err(cap_err)?;
    Ok((lease, revision))
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
    run_sandboxed_with(&build_manager(), root, argv, timeout, output_limit)
}

/// [`run_sandboxed`] against a caller-supplied manager. Split out for
/// `rapid doctor`'s sandbox smoke probe, which must exercise whichever
/// backend *this platform's* production `shell_exec` would actually pick
/// ([`build_manager_seatbelt`] on a macOS host with `sandbox-exec` present,
/// [`build_manager`] everywhere else) rather than always the host-restricted
/// one this function's own caller hard-codes. Nothing else differs: the same
/// spec, the same lease-minting ceremony, the same validator, the same
/// destroy-after-exec cleanup — so a green probe is evidence about the real
/// execution path, not about a doctor-only reimplementation of it.
pub fn run_sandboxed_with(
    manager: &SandboxManager,
    root: &Path,
    argv: &[String],
    timeout: Duration,
    output_limit: u64,
) -> Result<SandboxRunOutcome, SandboxRunError> {
    let spec = build_spec(root, timeout, output_limit, SandboxNetwork::None)?;
    let issuer = LeaseIssuer::ephemeral();
    let command_name = argv.first().map(String::as_str).unwrap_or("shell");
    let (lease, revision) = mint_proc_exec_lease(&issuer, command_name)?;
    // `validate_use` is the mandatory pre-effect guard `SandboxManager::exec`
    // now requires — real, atomic use-count decrementing, not just a re-read
    // of the frozen snapshot `lease.remaining_uses()` was minted with. Built
    // from the exact same issuer/revision the lease above was minted under,
    // so its MAC and policy-revision checks agree with what's actually baked
    // into the lease.
    let validator = LeaseValidator::new(issuer, revision);
    let cancel = CancellationToken::new();

    // Both fallible steps that do not need a handle happen *before*
    // `prepare`: a `?` between `prepare` and the `destroy` below would leak a
    // prepared sandbox handle (an unresolvable `argv[0]` was enough to do it).
    let program = resolve_program(root, command_name)?;
    let resolved_argv = std::iter::once(program).chain(argv.iter().skip(1).cloned());
    let request = SandboxExecRequest::new(resolved_argv, timeout, output_limit)
        .map_err(SandboxRunError::Sandbox)?;
    let handle = manager
        .prepare(&spec, &lease, &cancel)
        .map_err(SandboxRunError::Sandbox)?;
    let result = manager.exec(&spec, &handle, &request, &lease, &validator, &cancel);
    // Best-effort cleanup: a destroy failure after a successful/failed exec
    // is a resource-leak concern for the backend's own bookkeeping, not
    // something the caller can act on — never mask the exec outcome with it.
    let _ = manager.destroy(&handle, &cancel);
    let result = result.map_err(SandboxRunError::Sandbox)?;

    Ok(SandboxRunOutcome {
        exit_code: result.exit().code(),
        timed_out: result.exit().timed_out(),
        signal: result.exit().signal(),
        oom: result.exit().oom(),
        policy_violation: result.exit().policy_violation(),
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
        protocol::host_path::canonicalize(&root).expect("canonicalize")
    }

    #[test]
    fn run_sandboxed_executes_and_captures_real_output() {
        let root = temp_root("basic");
        let argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "echo hello-from-sandbox".to_owned(),
        ];
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
        let outcome = run_sandboxed(&root, &argv, Duration::from_millis(300), 4096).expect("run");
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
