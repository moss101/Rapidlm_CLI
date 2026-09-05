//! Argv-first supervised process spawn.
//!
//! Children start in a dedicated process group/job with an explicit cwd, an
//! allowlisted environment, bounded stdin, and a [`JobId`]. Shell-string
//! execution is never inferred from argv. The lease guard is consumed
//! immediately before the OS spawn side effect, and only after the guard's
//! `action_hash` is re-checked against this [`ExecSpec`].

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use auth::SecretAwareValue;
use capability_broker::normalize::command::{
    MAX_ARGV, MAX_ARG_BYTES, MAX_ENV_NAMES, MAX_ENV_NAME_BYTES, MAX_PATH_BYTES,
    MAX_SHELL_SCRIPT_BYTES,
};
use capability_broker::{
    ActionRequest, CancellationToken, CanonicalAction, CanonicalCommand, CanonicalHostPath,
    Capability, CommandNormalizeError, ExecIntent, LeaseUseGuard, LiveHostResolver, PrincipalRef,
    ProcessScope, ResourceDescriptor, ShellMode, normalize_exec,
};
use protocol::{ArtifactId, ErrorCode, JobId, LeaseId, SessionId};

/// Maximum stdin bytes accepted at spawn. Larger payloads must become artifacts.
pub const MAX_STDIN_BYTES: usize = 64 * 1024;

/// Maximum process timeout accepted at spawn.
///
/// `cancel::await_exit` computes `job.started_at() + limit` (an `Instant + Duration`
/// addition that panics on overflow). No real caller needs anything close to this —
/// the largest today is `external_agents::DEFAULT_AGENT_TIMEOUT` at 600s — bounding it
/// here keeps an unreasonably large caller-supplied timeout from ever reaching that
/// addition instead of relying on every caller independently choosing a safe value.
pub const MAX_PROC_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Process-scope command family that is the distinct high-risk shell grant.
///
/// `Invocation::Shell` / [`ShellMode::ShellString`] is refused unless the bound
/// resource is `Process` with this family. Generic `proc.exec` families are not
/// a shell grant (T-002).
pub const SHELL_COMMAND_FAMILY: &str = "shell";

/// Binding tag copied from the capability-broker action fingerprint so spawn
/// can independently re-hash `ExecSpec` and compare to `LeaseUseGuard`.
const ACTION_BINDING_TAG: &[u8] = b"rapidlm.approval.binding.v1";

/// Env binding used by [`ExecSpec`]. Handles are not materialized here.
pub type SecretOrValue = SecretAwareValue;

/// Principal/session/capability/resource copied from the approved request.
///
/// Spawn re-hashes these fields plus the normalized command and compares the
/// digest to [`LeaseUseGuard::action_hash`]. A caller cannot retarget a
/// command-A guard onto command-B by mutating argv after issuance.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecBinding {
    principal: PrincipalRef,
    session_id: SessionId,
    capability: Capability,
    resource: ResourceDescriptor,
}

/// How the child is invoked. The arms are not interchangeable.
#[derive(Clone, Eq, PartialEq)]
pub enum Invocation {
    /// Direct exec. `argv[0]` is the resolved executable.
    Argv { argv: Vec<String> },
    /// Explicit shell-string mode. Requires a distinct high-risk capability.
    Shell { shell: String, script: String },
}

/// Normalized spawn request. Supervisor never parses model intent.
#[derive(Clone)]
pub struct ExecSpec {
    invocation: Invocation,
    cwd: CanonicalHostPath,
    env: BTreeMap<String, SecretOrValue>,
    stdin: StdinSpec,
    timeout: Option<Duration>,
    output_limit: u64,
    cancel: CancellationToken,
    binding: Option<ExecBinding>,
}

/// Child stdin. Parent stdin is never inherited.
#[derive(Clone, Eq, PartialEq)]
pub enum StdinSpec {
    Empty,
    Bytes(Vec<u8>),
}

/// OS process-group / job-leader identity recorded at spawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessGroupId(u32);

/// Live supervised child. Output spool and tree cancel are later modules.
pub struct JobHandle {
    job_id: JobId,
    lease_id: LeaseId,
    pid: u32,
    process_group_id: ProcessGroupId,
    mode: ShellMode,
    timeout: Option<Duration>,
    output_limit: u64,
    started_at: Instant,
    child: Child,
}

/// Typed spawn failure. Display never echoes argv, script, env, or stdin.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SpawnError {
    Cancelled,
    EmptyArgv,
    EmptyExecutable,
    EmptyCwd,
    EmptyEnvName,
    EmptyShell,
    EmptyScript,
    TooLong,
    TooManyArgs,
    TooManyEnvNames,
    Nul,
    Control,
    Unc,
    Traversal,
    InvalidEnvName,
    UnresolvedExecutable,
    UnresolvedCwd,
    RelativeExecutable,
    NotADirectory,
    SecretNotMaterialized,
    TimeoutInvalid,
    StdinTooLarge,
    LeaseNotBound,
    ShellGrantRequired,
    Io,
}

impl ExecBinding {
    /// Bind a `proc.exec` process scope. Other families fail closed.
    pub fn new(
        principal: PrincipalRef,
        session_id: SessionId,
        capability: Capability,
        resource: ResourceDescriptor,
    ) -> Result<Self, SpawnError> {
        if capability != Capability::ProcExec {
            return Err(SpawnError::LeaseNotBound);
        }
        if !matches!(resource, ResourceDescriptor::Process(_)) {
            return Err(SpawnError::LeaseNotBound);
        }
        Ok(Self {
            principal,
            session_id,
            capability,
            resource,
        })
    }

    /// Argv-family `proc.exec` binding. `command_family` is not `"shell"`.
    pub fn proc_exec(
        principal: PrincipalRef,
        session_id: SessionId,
        command_family: &str,
    ) -> Result<Self, SpawnError> {
        if command_family == SHELL_COMMAND_FAMILY {
            return Err(SpawnError::ShellGrantRequired);
        }
        let resource = ResourceDescriptor::Process(
            ProcessScope::new(command_family).map_err(|_| SpawnError::LeaseNotBound)?,
        );
        Self::new(principal, session_id, Capability::ProcExec, resource)
    }

    /// Distinct high-risk shell-string grant (`command_family = "shell"`).
    pub fn shell_grant(principal: PrincipalRef, session_id: SessionId) -> Result<Self, SpawnError> {
        let resource = ResourceDescriptor::Process(
            ProcessScope::new(SHELL_COMMAND_FAMILY).map_err(|_| SpawnError::LeaseNotBound)?,
        );
        Self::new(principal, session_id, Capability::ProcExec, resource)
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn command_family(&self) -> Option<&str> {
        match &self.resource {
            ResourceDescriptor::Process(scope) => Some(scope.command_family().as_str()),
            _ => None,
        }
    }

    pub fn is_shell_grant(&self) -> bool {
        self.command_family() == Some(SHELL_COMMAND_FAMILY)
    }
}

impl ExecSpec {
    /// Argv-first spec. Tokens are not joined and no shell is selected.
    pub fn argv(
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: CanonicalHostPath,
        env: impl IntoIterator<Item = (impl Into<String>, SecretOrValue)>,
        stdin: StdinSpec,
        timeout: Option<Duration>,
        output_limit: u64,
        cancel: CancellationToken,
    ) -> Result<Self, SpawnError> {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        Self::build(
            Invocation::Argv { argv },
            cwd,
            env,
            stdin,
            timeout,
            output_limit,
            cancel,
        )
    }

    /// Explicit shell-string spec. Distinct from argv even when argv is `sh -c`.
    #[allow(clippy::too_many_arguments)]
    pub fn shell(
        shell: impl Into<String>,
        script: impl Into<String>,
        cwd: CanonicalHostPath,
        env: impl IntoIterator<Item = (impl Into<String>, SecretOrValue)>,
        stdin: StdinSpec,
        timeout: Option<Duration>,
        output_limit: u64,
        cancel: CancellationToken,
    ) -> Result<Self, SpawnError> {
        Self::build(
            Invocation::Shell {
                shell: shell.into(),
                script: script.into(),
            },
            cwd,
            env,
            stdin,
            timeout,
            output_limit,
            cancel,
        )
    }

    /// Attach the approved request binding. Required before [`spawn`].
    pub fn bind(mut self, binding: ExecBinding) -> Result<Self, SpawnError> {
        self.validate_binding(&binding)?;
        self.binding = Some(binding);
        Ok(self)
    }

    pub fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    pub fn mode(&self) -> ShellMode {
        match self.invocation {
            Invocation::Argv { .. } => ShellMode::Argv,
            Invocation::Shell { .. } => ShellMode::ShellString,
        }
    }

    pub fn cwd(&self) -> &CanonicalHostPath {
        &self.cwd
    }

    pub fn env(&self) -> &BTreeMap<String, SecretOrValue> {
        &self.env
    }

    pub fn stdin(&self) -> &StdinSpec {
        &self.stdin
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn output_limit(&self) -> u64 {
        self.output_limit
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn binding(&self) -> Option<&ExecBinding> {
        self.binding.as_ref()
    }

    fn build(
        invocation: Invocation,
        cwd: CanonicalHostPath,
        env: impl IntoIterator<Item = (impl Into<String>, SecretOrValue)>,
        stdin: StdinSpec,
        timeout: Option<Duration>,
        output_limit: u64,
        cancel: CancellationToken,
    ) -> Result<Self, SpawnError> {
        let spec = Self {
            invocation,
            cwd,
            env: collect_env(env)?,
            stdin,
            timeout,
            output_limit,
            cancel,
            binding: None,
        };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), SpawnError> {
        if self.cancel.is_cancelled() {
            return Err(SpawnError::Cancelled);
        }
        if let Some(limit) = self.timeout {
            if limit == Duration::ZERO || limit > MAX_PROC_TIMEOUT {
                return Err(SpawnError::TimeoutInvalid);
            }
        }
        match &self.stdin {
            StdinSpec::Empty => {}
            StdinSpec::Bytes(bytes) => {
                if bytes.len() > MAX_STDIN_BYTES {
                    return Err(SpawnError::StdinTooLarge);
                }
            }
        }
        validate_env_map(&self.env)?;
        match &self.invocation {
            Invocation::Argv { argv } => validate_argv(argv)?,
            Invocation::Shell { shell, script } => {
                if shell.is_empty() {
                    return Err(SpawnError::EmptyShell);
                }
                if script.is_empty() {
                    return Err(SpawnError::EmptyScript);
                }
                validate_text(shell, MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
                validate_text(script, MAX_SHELL_SCRIPT_BYTES, ControlPolicy::AllowShell)?;
                require_absolute_host_path(shell)?;
            }
        }
        if let Some(binding) = &self.binding {
            self.validate_binding(binding)?;
        }
        Ok(())
    }

    fn validate_binding(&self, binding: &ExecBinding) -> Result<(), SpawnError> {
        if binding.capability != Capability::ProcExec {
            return Err(SpawnError::LeaseNotBound);
        }
        let family = match &binding.resource {
            ResourceDescriptor::Process(scope) => scope.command_family().as_str(),
            _ => return Err(SpawnError::LeaseNotBound),
        };
        match self.mode() {
            ShellMode::ShellString if family != SHELL_COMMAND_FAMILY => {
                Err(SpawnError::ShellGrantRequired)
            }
            ShellMode::Argv if family == SHELL_COMMAND_FAMILY => Err(SpawnError::ShellGrantRequired),
            _ => Ok(()),
        }
    }

    fn canonical_command(&self) -> Result<CanonicalCommand, SpawnError> {
        let env_names = self.env.keys().cloned();
        let intent = match &self.invocation {
            Invocation::Argv { argv } => {
                ExecIntent::argv(argv.clone(), self.cwd.as_str().to_owned(), env_names)
            }
            Invocation::Shell { shell, script } => ExecIntent::shell(
                shell.clone(),
                script.clone(),
                self.cwd.as_str().to_owned(),
                env_names,
            ),
        };
        normalize_exec(&intent, &LiveHostResolver, &self.cancel).map_err(map_normalize)
    }
}

impl JobHandle {
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn process_group_id(&self) -> ProcessGroupId {
        self.process_group_id
    }

    pub fn mode(&self) -> ShellMode {
        self.mode
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn output_limit(&self) -> u64 {
        self.output_limit
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn child(&self) -> &Child {
        &self.child
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    pub fn wait_with_output(self) -> std::io::Result<Output> {
        self.child.wait_with_output()
    }
}

impl ProcessGroupId {
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Spawn a child from a normalized spec after consuming the lease guard.
///
/// Fails closed unless `lease.action_hash()` equals the independently
/// recomputed digest of this spec's bound `proc.exec` command. Shell-string
/// mode additionally requires [`SHELL_COMMAND_FAMILY`]. The child environment
/// contains only `spec.env` plaintext entries. Secret handles fail closed.
/// Parent env, stdin, and process group are not inherited.
pub fn spawn(spec: ExecSpec, lease: LeaseUseGuard) -> Result<JobHandle, SpawnError> {
    spec.validate()?;
    let prepared = Prepared::from_spec(&spec)?;
    verify_lease_bound(&spec, &lease)?;
    if spec.cancel.is_cancelled() {
        return Err(SpawnError::Cancelled);
    }

    let consumed = lease.consume();
    let mut command = prepared.command(&spec.stdin);
    let mut child = command.spawn().map_err(|_| SpawnError::Io)?;
    let pid = child.id();
    if let Err(err) = write_stdin(&mut child, &spec.stdin) {
        // `child.kill()` only signals the leader PID. `isolate_process_group`
        // put this child in its own process group (pgid == pid), so any
        // grandchild it already forked before this failure — an ordinary
        // shell command that backgrounds work, or one that exits without
        // draining all of stdin — would otherwise survive as an orphan with
        // no `JobHandle` ever handed back to find or kill it later. Signal
        // the whole group too, best-effort, matching `terminate_tree`'s own
        // group-based termination model.
        let _ = crate::cancel::signal_group(ProcessGroupId(pid), crate::cancel::SignalKind::Kill);
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    Ok(JobHandle {
        job_id: JobId::new(),
        lease_id: consumed.lease_id(),
        pid,
        process_group_id: ProcessGroupId(pid),
        mode: spec.mode(),
        timeout: spec.timeout,
        output_limit: spec.output_limit,
        started_at: Instant::now(),
        child,
    })
}

/// Re-hash the spec as `CanonicalAction::Command` and compare to the guard.
fn verify_lease_bound(spec: &ExecSpec, lease: &LeaseUseGuard) -> Result<(), SpawnError> {
    let binding = spec.binding.as_ref().ok_or(SpawnError::LeaseNotBound)?;
    spec.validate_binding(binding)?;
    let command = spec.canonical_command()?;
    let request = ActionRequest::new(
        binding.principal.clone(),
        binding.session_id,
        binding.capability,
        binding.resource.clone(),
        CanonicalAction::Command(command),
        "",
    )
    .map_err(|_| SpawnError::LeaseNotBound)?;
    let expected = action_digest(&request);
    if !ct_eq(&expected, lease.action_hash().as_bytes()) {
        return Err(SpawnError::LeaseNotBound);
    }
    Ok(())
}

/// Same digest as `ActionFingerprint::of` for an `ActionRequest`.
fn action_digest(request: &ActionRequest) -> [u8; 32] {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(ACTION_BINDING_TAG);
    buf.push(0);
    buf.extend_from_slice(request.principal().as_str().as_bytes());
    buf.push(0);
    buf.extend_from_slice(request.session_id().to_string().as_bytes());
    buf.push(0);
    buf.extend_from_slice(request.capability().as_str().as_bytes());
    buf.push(0);
    append_resource_bytes(&mut buf, request.resource());
    buf.push(0);
    append_action_bytes(&mut buf, request.normalized_action());
    *ArtifactId::from_bytes(&buf).as_digest()
}

fn append_action_bytes(buf: &mut Vec<u8>, action: &CanonicalAction) {
    match action {
        CanonicalAction::Command(command) => {
            buf.extend_from_slice(b"command\0");
            buf.extend_from_slice(&command.policy_bytes());
        }
        CanonicalAction::Filesystem(fs) => {
            buf.extend_from_slice(b"fs\0");
            buf.extend_from_slice(&fs.policy_bytes());
        }
        CanonicalAction::Network(net) => {
            buf.extend_from_slice(b"net\0");
            buf.extend_from_slice(&net.policy_bytes());
        }
        CanonicalAction::Resource {
            capability,
            resource,
        } => {
            buf.extend_from_slice(b"resource\0");
            buf.extend_from_slice(capability.as_str().as_bytes());
            buf.push(0);
            append_resource_bytes(buf, resource);
        }
    }
}

fn append_resource_bytes(buf: &mut Vec<u8>, resource: &ResourceDescriptor) {
    match resource {
        ResourceDescriptor::Filesystem(scope) => {
            buf.extend_from_slice(b"fs\0");
            buf.extend_from_slice(scope.root().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.glob().as_str().as_bytes());
        }
        ResourceDescriptor::Process(scope) => {
            buf.extend_from_slice(b"proc\0");
            buf.extend_from_slice(scope.command_family().as_str().as_bytes());
        }
        ResourceDescriptor::Network(scope) => {
            buf.extend_from_slice(b"net\0");
            buf.extend_from_slice(scope.scheme().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.host().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.port().to_string().as_bytes());
        }
        ResourceDescriptor::Git(scope) => {
            buf.extend_from_slice(b"git\0");
            buf.extend_from_slice(scope.ref_scope().as_str().as_bytes());
        }
        ResourceDescriptor::Secret(scope) => {
            buf.extend_from_slice(b"secret\0");
            buf.extend_from_slice(scope.secret_id().as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.target().as_str().as_bytes());
        }
        ResourceDescriptor::Browser(scope) => {
            buf.extend_from_slice(b"browser\0");
            buf.extend_from_slice(scope.origin().to_string().as_bytes());
            buf.push(0);
            if let Some(path) = scope.path() {
                buf.extend_from_slice(path.as_str().as_bytes());
            }
        }
        ResourceDescriptor::Mobile(scope) => {
            buf.extend_from_slice(b"mobile\0");
            buf.extend_from_slice(scope.device_id().as_str().as_bytes());
        }
        ResourceDescriptor::Mcp(scope) => {
            buf.extend_from_slice(b"mcp\0");
            buf.extend_from_slice(scope.server().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.tool().as_bytes());
        }
        ResourceDescriptor::Plugin(scope) => {
            buf.extend_from_slice(b"plugin\0");
            buf.extend_from_slice(scope.plugin().as_bytes());
            buf.push(0);
            buf.extend_from_slice(scope.capability().as_bytes());
        }
        _ => buf.extend_from_slice(b"unknown\0"),
    }
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut acc = 0u8;
    for i in 0..32 {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}


struct Prepared {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

impl Prepared {
    fn from_spec(spec: &ExecSpec) -> Result<Self, SpawnError> {
        let cwd = host_path(spec.cwd.as_str());
        if !cwd.is_dir() {
            return Err(if cwd.exists() {
                SpawnError::NotADirectory
            } else {
                SpawnError::UnresolvedCwd
            });
        }

        let (program, args) = match &spec.invocation {
            Invocation::Argv { argv } => (host_executable(&argv[0])?, argv[1..].to_vec()),
            Invocation::Shell { shell, script } => (
                host_executable(shell)?,
                vec!["-c".to_owned(), script.clone()],
            ),
        };

        let mut env = BTreeMap::new();
        for (name, value) in &spec.env {
            match value {
                SecretAwareValue::Plaintext(text) => {
                    env.insert(name.clone(), text.clone());
                }
                SecretAwareValue::Handle(_) => return Err(SpawnError::SecretNotMaterialized),
            }
        }

        Ok(Self {
            program,
            args,
            cwd,
            env,
        })
    }

    fn command(&self, stdin: &StdinSpec) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.current_dir(&self.cwd);
        command.env_clear();
        command.envs(&self.env);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        match stdin {
            StdinSpec::Empty => {
                command.stdin(Stdio::null());
            }
            StdinSpec::Bytes(_) => {
                command.stdin(Stdio::piped());
            }
        }
        isolate_process_group(&mut command);
        command
    }
}

fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

fn write_stdin(child: &mut Child, stdin: &StdinSpec) -> Result<(), SpawnError> {
    match stdin {
        StdinSpec::Empty => Ok(()),
        StdinSpec::Bytes(bytes) => {
            let Some(pipe) = child.stdin.as_mut() else {
                return Err(SpawnError::Io);
            };
            pipe.write_all(bytes).map_err(|_| SpawnError::Io)?;
            let _ = child.stdin.take();
            Ok(())
        }
    }
}

fn collect_env(
    env: impl IntoIterator<Item = (impl Into<String>, SecretOrValue)>,
) -> Result<BTreeMap<String, SecretOrValue>, SpawnError> {
    let mut map = BTreeMap::new();
    for (name, value) in env {
        let name = name.into();
        validate_env_name(&name)?;
        if let SecretAwareValue::Plaintext(text) = &value {
            if text.contains('\0') {
                return Err(SpawnError::Nul);
            }
            if text.len() > auth::MAX_SECRET_BYTES {
                return Err(SpawnError::TooLong);
            }
        }
        map.insert(name, value);
    }
    if map.len() > MAX_ENV_NAMES {
        return Err(SpawnError::TooManyEnvNames);
    }
    Ok(map)
}

fn validate_env_map(env: &BTreeMap<String, SecretOrValue>) -> Result<(), SpawnError> {
    if env.len() > MAX_ENV_NAMES {
        return Err(SpawnError::TooManyEnvNames);
    }
    for (name, value) in env {
        validate_env_name(name)?;
        if let SecretAwareValue::Plaintext(text) = value {
            if text.contains('\0') {
                return Err(SpawnError::Nul);
            }
            if text.len() > auth::MAX_SECRET_BYTES {
                return Err(SpawnError::TooLong);
            }
        }
    }
    Ok(())
}

fn validate_argv(argv: &[String]) -> Result<(), SpawnError> {
    if argv.is_empty() {
        return Err(SpawnError::EmptyArgv);
    }
    if argv.len() > MAX_ARGV {
        return Err(SpawnError::TooManyArgs);
    }
    if argv[0].is_empty() {
        return Err(SpawnError::EmptyExecutable);
    }
    validate_text(&argv[0], MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
    require_absolute_host_path(&argv[0])?;
    for arg in argv.iter().skip(1) {
        validate_text(arg, MAX_ARG_BYTES, ControlPolicy::RejectAll)?;
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), SpawnError> {
    if name.is_empty() {
        return Err(SpawnError::EmptyEnvName);
    }
    if name.len() > MAX_ENV_NAME_BYTES {
        return Err(SpawnError::TooLong);
    }
    if name.contains('\0') {
        return Err(SpawnError::Nul);
    }
    if name.chars().any(char::is_control) {
        return Err(SpawnError::Control);
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(SpawnError::InvalidEnvName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(SpawnError::InvalidEnvName);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ControlPolicy {
    RejectAll,
    AllowShell,
}

fn validate_text(value: &str, max_bytes: usize, controls: ControlPolicy) -> Result<(), SpawnError> {
    if value.len() > max_bytes {
        return Err(SpawnError::TooLong);
    }
    if value.contains('\0') {
        return Err(SpawnError::Nul);
    }
    match controls {
        ControlPolicy::RejectAll => {
            if value.chars().any(char::is_control) {
                return Err(SpawnError::Control);
            }
        }
        ControlPolicy::AllowShell => {
            if value
                .chars()
                .any(|c| char::is_control(c) && c != '\n' && c != '\t' && c != '\r')
            {
                return Err(SpawnError::Control);
            }
        }
    }
    Ok(())
}

fn require_absolute_host_path(path: &str) -> Result<CanonicalHostPath, SpawnError> {
    CanonicalHostPath::from_resolved(path).map_err(|err| match err {
        CommandNormalizeError::EmptyCwd => SpawnError::EmptyExecutable,
        CommandNormalizeError::TooLong => SpawnError::TooLong,
        CommandNormalizeError::Nul => SpawnError::Nul,
        CommandNormalizeError::Control => SpawnError::Control,
        CommandNormalizeError::Unc => SpawnError::Unc,
        CommandNormalizeError::Traversal => SpawnError::Traversal,
        CommandNormalizeError::UnresolvedCwd => SpawnError::RelativeExecutable,
        _ => SpawnError::UnresolvedExecutable,
    })
}

fn host_executable(path: &str) -> Result<PathBuf, SpawnError> {
    let canonical = require_absolute_host_path(path)?;
    let host = host_path(canonical.as_str());
    if !host.is_file() {
        return Err(SpawnError::UnresolvedExecutable);
    }
    Ok(host)
}

fn host_path(path: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(path)
    }
}

fn map_normalize(err: CommandNormalizeError) -> SpawnError {
    match err {
        CommandNormalizeError::Cancelled => SpawnError::Cancelled,
        CommandNormalizeError::EmptyExecutable => SpawnError::EmptyExecutable,
        CommandNormalizeError::EmptyArgv => SpawnError::EmptyArgv,
        CommandNormalizeError::EmptyCwd => SpawnError::EmptyCwd,
        CommandNormalizeError::EmptyEnvName => SpawnError::EmptyEnvName,
        CommandNormalizeError::EmptyShell => SpawnError::EmptyShell,
        CommandNormalizeError::EmptyScript => SpawnError::EmptyScript,
        CommandNormalizeError::TooLong => SpawnError::TooLong,
        CommandNormalizeError::TooManyArgs => SpawnError::TooManyArgs,
        CommandNormalizeError::TooManyEnvNames => SpawnError::TooManyEnvNames,
        CommandNormalizeError::Nul => SpawnError::Nul,
        CommandNormalizeError::Control => SpawnError::Control,
        CommandNormalizeError::Unc => SpawnError::Unc,
        CommandNormalizeError::Traversal => SpawnError::Traversal,
        CommandNormalizeError::InvalidEnvName => SpawnError::InvalidEnvName,
        CommandNormalizeError::UnresolvedExecutable => SpawnError::UnresolvedExecutable,
        CommandNormalizeError::UnresolvedCwd => SpawnError::UnresolvedCwd,
        CommandNormalizeError::SymlinkLoop => SpawnError::UnresolvedExecutable,
    }
}

impl SpawnError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "process spawn cancelled",
            Self::EmptyArgv => "argv is empty",
            Self::EmptyExecutable => "executable is empty",
            Self::EmptyCwd => "cwd is empty",
            Self::EmptyEnvName => "environment variable name is empty",
            Self::EmptyShell => "shell executable is empty",
            Self::EmptyScript => "shell script is empty",
            Self::TooLong => "spawn field exceeds bound",
            Self::TooManyArgs => "argv exceeds bound",
            Self::TooManyEnvNames => "environment map exceeds bound",
            Self::Nul => "spawn field contains NUL",
            Self::Control => "spawn field contains a control character",
            Self::Unc => "UNC path is not a local command path",
            Self::Traversal => "resolved spawn path contains a traversal component",
            Self::InvalidEnvName => "invalid environment variable name",
            Self::UnresolvedExecutable => "executable could not be resolved",
            Self::UnresolvedCwd => "cwd could not be resolved",
            Self::RelativeExecutable => "executable is not an absolute host path",
            Self::NotADirectory => "cwd is not a directory",
            Self::SecretNotMaterialized => {
                "secret handle cannot be injected without a broker token"
            }
            Self::TimeoutInvalid => "process timeout is invalid",
            Self::StdinTooLarge => "stdin exceeds bound",
            Self::LeaseNotBound => "lease is not bound to this exec spec",
            Self::ShellGrantRequired => "shell-string mode requires a distinct shell grant",
            Self::Io => "process spawn failed",
        }
    }

    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::TimeoutInvalid => Some(ErrorCode::ProcessTimeout),
            Self::SecretNotMaterialized => Some(ErrorCode::PolicyDenied),
            Self::LeaseNotBound => Some(ErrorCode::PolicyLeaseInvalid),
            Self::ShellGrantRequired => Some(ErrorCode::PolicyDenied),
            Self::Io => Some(ErrorCode::InternalUnexpected),
            _ => Some(ErrorCode::ToolInvalidArguments),
        }
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SpawnError {}

impl fmt::Debug for Invocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argv { argv } => f.debug_struct("Argv").field("len", &argv.len()).finish(),
            Self::Shell { .. } => f
                .debug_struct("Shell")
                .field("script", &"redacted")
                .finish(),
        }
    }
}

impl fmt::Debug for StdinSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.debug_tuple("Empty").finish(),
            Self::Bytes(bytes) => f.debug_tuple("Bytes").field(&bytes.len()).finish(),
        }
    }
}

impl fmt::Debug for ExecBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecBinding")
            .field("principal", &self.principal)
            .field("session_id", &self.session_id)
            .field("capability", &self.capability.as_str())
            .field("command_family", &self.command_family())
            .finish()
    }
}

impl fmt::Debug for ExecSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_keys: Vec<&str> = self.env.keys().map(String::as_str).collect();
        f.debug_struct("ExecSpec")
            .field("mode", &self.mode())
            .field("invocation", &self.invocation)
            .field("cwd", &self.cwd)
            .field("env_keys", &env_keys)
            .field("stdin", &self.stdin)
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("binding", &self.binding)
            .finish()
    }
}

impl fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobHandle")
            .field("job_id", &self.job_id)
            .field("lease_id", &self.lease_id)
            .field("pid", &self.pid)
            .field("process_group_id", &self.process_group_id)
            .field("mode", &self.mode)
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ProcessGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    use auth::SecretRef;
    use capability_broker::{
        evaluate, issue, request_approval, validate_use, ActionRequest, ApprovalChoice,
        ApprovalResolution, ApprovalScopeId, CanonicalAction, Capability, CapabilityLease,
        FilesystemScope, LeaseIssuer, LeaseValidator, PolicyDocument, PolicyRevision, PolicySource,
        PolicyStack, PrincipalRef, ProcessScope, ResourceDescriptor,
    };
    use protocol::SessionId;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-7c1e9b";

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new()).expect("parse")
    }

    fn proc_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "proc-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "test" }

[[rules]]
id = "proc-shell-allow"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "shell" }
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "proc-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"
"#,
                PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
            ),
        ])
        .expect("stack")
    }

    fn fs_stack() -> PolicyStack {
        PolicyStack::new([
            parse_doc(
                r#"
[[rules]]
id = "fs-allow"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
                PolicySource::user("user-policy.toml").expect("user"),
            ),
            parse_doc(
                r#"
[[rules]]
id = "fs-ask"
effect = "ask"
subjects = ["*"]
capability = "fs.read"
"#,
                PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
            ),
        ])
        .expect("stack")
    }

    fn issuer() -> LeaseIssuer {
        LeaseIssuer::from_key([0x11; 32]).expect("issuer")
    }

    fn approve_issue(
        request: &ActionRequest,
        policies: &PolicyStack,
        now: Instant,
    ) -> CapabilityLease {
        let decision = evaluate(policies, request, &CancellationToken::new()).expect("evaluate");
        let approval = request_approval(request, &decision, now, &CancellationToken::new())
            .expect("approval");
        let approved = match approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                request,
                now,
                &CancellationToken::new(),
            )
            .expect("resolve")
        {
            ApprovalResolution::Approved(approved) => approved,
            ApprovalResolution::Denied => panic!("expected approved"),
        };
        issue(
            &issuer(),
            &approved,
            policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue")
    }

    fn guard_for_action(
        policies: &PolicyStack,
        request: &ActionRequest,
        actual: &CanonicalAction,
    ) -> LeaseUseGuard {
        let now = Instant::now();
        let lease = approve_issue(request, policies, now);
        let validator = LeaseValidator::new(issuer(), PolicyRevision::of_stack(policies));
        validate_use(
            &validator,
            &lease,
            actual,
            now,
            &CancellationToken::new(),
        )
        .expect("guard")
    }

    fn lease_guard(spec: &ExecSpec) -> LeaseUseGuard {
        let binding = spec.binding().expect("bound spec");
        let command = spec.canonical_command().expect("canon");
        let actual = CanonicalAction::Command(command);
        let request = ActionRequest::new(
            binding.principal().clone(),
            binding.session_id(),
            binding.capability(),
            binding.resource().clone(),
            actual.clone(),
            "exec",
        )
        .expect("request");
        guard_for_action(&proc_stack(), &request, &actual)
    }

    fn resource_proc_guard(session: SessionId, family: &str) -> LeaseUseGuard {
        let resource = ResourceDescriptor::Process(ProcessScope::new(family).expect("process"));
        let actual = CanonicalAction::Resource {
            capability: Capability::ProcExec,
            resource: resource.clone(),
        };
        let request = ActionRequest::new(
            principal(),
            session,
            Capability::ProcExec,
            resource,
            actual.clone(),
            "exec",
        )
        .expect("request");
        guard_for_action(&proc_stack(), &request, &actual)
    }

    fn fs_read_guard() -> LeaseUseGuard {
        let resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs"));
        let actual = CanonicalAction::Resource {
            capability: Capability::FsRead,
            resource: resource.clone(),
        };
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            resource,
            actual.clone(),
            "read",
        )
        .expect("request");
        guard_for_action(&fs_stack(), &request, &actual)
    }

    fn temp_cwd() -> CanonicalHostPath {
        let tmp = std::env::temp_dir().canonicalize().expect("temp");
        let rendered = tmp.to_str().expect("utf8 temp").replace('\\', "/");
        CanonicalHostPath::from_resolved(&rendered).expect("cwd")
    }

    fn require_bin(path: &str) -> String {
        assert!(
            Path::new(path).is_file(),
            "missing test fixture binary {path}"
        );
        path.to_owned()
    }

    fn test_binding() -> ExecBinding {
        ExecBinding::proc_exec(principal(), SessionId::new(), "test").expect("binding")
    }

    fn shell_binding() -> ExecBinding {
        ExecBinding::shell_grant(principal(), SessionId::new()).expect("shell binding")
    }

    fn argv_spec(argv: &[&str]) -> ExecSpec {
        ExecSpec::argv(
            argv.iter().copied(),
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec")
        .bind(test_binding())
        .expect("bind")
    }

    fn env_spec(argv: &[&str], env: Vec<(&str, SecretOrValue)>) -> ExecSpec {
        ExecSpec::argv(
            argv.iter().copied(),
            temp_cwd(),
            env.into_iter().map(|(k, v)| (k.to_owned(), v)),
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec")
        .bind(test_binding())
        .expect("bind")
    }

    fn shell_spec(shell: String, script: &str) -> ExecSpec {
        ExecSpec::shell(
            shell,
            script,
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("shell spec")
        .bind(shell_binding())
        .expect("shell bind")
    }

    #[cfg(unix)]
    fn query_pgid(pid: u32) -> u32 {
        let output = Command::new("/bin/ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        assert!(output.status.success(), "ps failed");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("pgid")
    }

    #[cfg(unix)]
    #[test]
    fn spawn_assigns_lifecycle_ids_and_dedicated_process_group() {
        let sleep = require_bin("/bin/sleep");
        let spec = argv_spec(&[&sleep, "5"]);
        let mut handle = spawn(spec.clone(), lease_guard(&spec)).expect("spawn");
        assert_ne!(handle.job_id().to_string(), handle.lease_id().to_string());
        assert_eq!(handle.process_group_id().as_u32(), handle.pid());
        assert_eq!(handle.mode(), ShellMode::Argv);
        assert_eq!(query_pgid(handle.pid()), handle.pid());
        handle.kill().expect("kill");
        let _ = handle.wait_with_output();
    }

    #[cfg(unix)]
    #[test]
    fn child_inherits_only_explicit_env_and_cwd() {
        let env_bin = require_bin("/usr/bin/env");
        let marker = SecretAwareValue::plaintext("only-child").expect("plain");
        let spec = env_spec(&[&env_bin], vec![("RAPIDLM_SPAWN_MARK", marker)]);
        let cwd = spec.cwd().as_str().to_owned();
        let output = spawn(spec.clone(), lease_guard(&spec))
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "RAPIDLM_SPAWN_MARK=only-child");
        assert!(!stdout.contains("PATH="));
        assert!(!stdout.contains("HOME="));

        let pwd = require_bin("/bin/pwd");
        let spec = argv_spec(&[&pwd]);
        let output = spawn(spec.clone(), lease_guard(&spec))
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        let got = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let expected = Path::new(&cwd)
            .canonicalize()
            .expect("canon cwd")
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }

    #[cfg(unix)]
    #[test]
    fn stdin_bytes_are_delivered_and_parent_stdin_is_not_inherited() {
        let cat = require_bin("/bin/cat");
        let spec = ExecSpec::argv(
            [cat],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Bytes(b"hello-stdin".to_vec()),
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec")
        .bind(test_binding())
        .expect("bind");
        let output = spawn(spec.clone(), lease_guard(&spec))
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        assert_eq!(output.stdout, b"hello-stdin");
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    // `spawn()`'s stdin-write-failure cleanup (below) now signals the whole
    // process group via `crate::cancel::signal_group`, not just the leader
    // PID, so a grandchild the leader already forked doesn't survive as an
    // orphan. The natural trigger for that cleanup path — a broken-pipe
    // write failure — turned out to be unwinnable as a black-box test on
    // this platform: writing up to `MAX_STDIN_BYTES` (65536, empirically
    // exactly this system's pipe capacity) into a freshly-created pipe
    // always completes in one non-blocking syscall regardless of whether
    // the child ever reads it, and even a child that closes its own stdin
    // as the very first thing it does never gets scheduled before the
    // parent's write already returned (verified empirically: 0/30 forced
    // failures across two independent race constructions). So this test
    // instead verifies the mechanism the fix relies on directly: that
    // `signal_group` — now `pub(crate)` so `spawn()` can call it — actually
    // reaches a grandchild inside the target process group, using a real
    // group obtained through the crate's own normal `spawn()` path.
    #[cfg(unix)]
    #[test]
    fn signal_group_kill_reaches_a_grandchild_not_just_the_leader() {
        let sh = require_bin("/bin/sh");
        let pidfile = std::env::temp_dir().join(format!(
            "psup-spawn-signalgroup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&pidfile);
        // Background a grandchild inside the leader's own process group
        // (isolate_process_group puts every spawned child in its own new
        // group), record its pid, then the leader itself stays alive too so
        // this exercises killing a live group with more than one member.
        let script = format!("sleep 30 & echo $! > {} ; sleep 30", pidfile.display());
        let spec = ExecSpec::shell(
            sh,
            script,
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec")
        .bind(shell_binding())
        .expect("bind");
        let mut job = spawn(spec.clone(), lease_guard(&spec)).expect("spawn");
        let leader_pid = job.pid();
        let process_group_id = job.process_group_id();

        let deadline = Instant::now() + Duration::from_secs(2);
        let grandchild_pid: u32 = loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "grandchild pid file was never written");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(pid_alive(leader_pid), "leader should still be running");
        assert!(pid_alive(grandchild_pid), "grandchild should still be running");

        crate::cancel::signal_group(process_group_id, crate::cancel::SignalKind::Kill)
            .expect("signal group");
        // Reap the leader ourselves (we're its direct parent — Rust's
        // `Child` never waits on drop, and an unreaped killed process stays
        // a zombie that `kill -0` still reports as "alive"). The grandchild
        // has no such issue: once orphaned by the killed leader it's
        // reparented to init, which reaps it on its own.
        job.child_mut().wait().expect("reap leader");

        let deadline = Instant::now() + Duration::from_secs(2);
        while pid_alive(grandchild_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(!pid_alive(leader_pid), "leader survived a group kill");
        assert!(
            !pid_alive(grandchild_pid),
            "grandchild pid {grandchild_pid} survived a group kill — this is exactly what \
             `child.kill()` alone (single-PID, the pre-fix cleanup) would have missed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn argv_metacharacters_are_not_implicit_shell() {
        let echo = require_bin("/bin/echo");
        let spec = argv_spec(&[&echo, "hello; echo pwned"]);
        let output = spawn(spec.clone(), lease_guard(&spec))
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout, "hello; echo pwned\n");
        assert_eq!(stdout.lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_shell_mode_is_distinct_from_argv_sh_c() {
        let sh = require_bin("/bin/sh");
        let argv_only = argv_spec(&[&sh, "-c", "echo ARGV_MODE"]);
        assert_eq!(argv_only.mode(), ShellMode::Argv);
        let argv_out = spawn(argv_only.clone(), lease_guard(&argv_only))
            .expect("spawn argv")
            .wait_with_output()
            .expect("wait");
        assert_eq!(
            String::from_utf8_lossy(&argv_out.stdout).trim(),
            "ARGV_MODE"
        );

        let spec = shell_spec(sh, "echo SHELL_MODE");
        assert_eq!(spec.mode(), ShellMode::ShellString);
        assert!(spec.binding().expect("bound").is_shell_grant());
        let handle = spawn(spec.clone(), lease_guard(&spec)).expect("spawn shell");
        assert_eq!(handle.mode(), ShellMode::ShellString);
        let shell_out = handle.wait_with_output().expect("wait");
        assert_eq!(
            String::from_utf8_lossy(&shell_out.stdout).trim(),
            "SHELL_MODE"
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_a_or_non_proc_guard_cannot_spawn_command_b() {
        let echo = require_bin("/bin/echo");
        let sleep = require_bin("/bin/sleep");
        let session = SessionId::new();
        let binding = ExecBinding::proc_exec(principal(), session, "test").expect("binding");
        let spec_a = ExecSpec::argv(
            [echo.clone(), "A".to_owned()],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec a")
        .bind(binding.clone())
        .expect("bind a");
        let spec_b = ExecSpec::argv(
            [sleep, "1".to_owned()],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec b")
        .bind(binding)
        .expect("bind b");

        let err = spawn(spec_b.clone(), lease_guard(&spec_a)).expect_err("a->b");
        assert_eq!(err, SpawnError::LeaseNotBound);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyLeaseInvalid));
        assert!(!err.to_string().contains("sleep"));

        let err = spawn(spec_b, fs_read_guard()).expect_err("fs->b");
        assert_eq!(err, SpawnError::LeaseNotBound);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyLeaseInvalid));
    }

    #[cfg(unix)]
    #[test]
    fn generic_proc_exec_resource_lease_cannot_spawn_shell() {
        let sh = require_bin("/bin/sh");
        let spec = shell_spec(sh, "echo pwned");
        let err = spawn(spec, resource_proc_guard(SessionId::new(), "test")).expect_err("shell");
        assert_eq!(err, SpawnError::LeaseNotBound);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyLeaseInvalid));
    }

    #[test]
    fn shell_string_without_shell_grant_is_refused() {
        let sh = if Path::new("/bin/sh").is_file() {
            "/bin/sh"
        } else {
            return;
        };
        let spec = ExecSpec::shell(
            sh,
            "echo pwned",
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("shell spec");
        let err = spec.bind(test_binding()).expect_err("generic proc");
        assert_eq!(err, SpawnError::ShellGrantRequired);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyDenied));
        assert!(!err.to_string().contains("pwned"));
    }

    #[test]
    fn relative_executable_fails_closed_without_path_search() {
        let err = ExecSpec::argv(
            ["echo"],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect_err("relative");
        assert_eq!(err, SpawnError::RelativeExecutable);
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        assert!(!err.to_string().contains("echo"));
    }

    #[test]
    fn secret_handle_env_fails_closed_and_is_redacted() {
        let echo = if Path::new("/bin/echo").is_file() {
            "/bin/echo"
        } else {
            return;
        };
        let handle = SecretAwareValue::handle(SecretRef::from_alias("env:CANARY").expect("ref"));
        let spec = env_spec(&[echo, "ok"], vec![("TOKEN", handle)]);
        let debug = format!("{spec:?}");
        assert!(!debug.contains(CANARY));
        assert!(!debug.contains("env:CANARY"));
        let err = spawn(spec.clone(), lease_guard(&spec)).expect_err("handle");
        assert_eq!(err, SpawnError::SecretNotMaterialized);
        assert_eq!(err.error_code(), Some(ErrorCode::PolicyDenied));
        assert!(!err.to_string().contains(CANARY));
    }

    #[test]
    fn cancelled_spawn_fails_closed_before_side_effect() {
        let echo = if Path::new("/bin/echo").is_file() {
            "/bin/echo"
        } else {
            return;
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = ExecSpec::argv(
            [echo],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            cancel,
        )
        .expect_err("cancelled construct");
        assert_eq!(err, SpawnError::Cancelled);
        assert_eq!(err.error_code(), None);

        let cancel = CancellationToken::new();
        let spec = ExecSpec::argv(
            [echo],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            cancel.clone(),
        )
        .expect("spec")
        .bind(test_binding())
        .expect("bind");
        let guard = lease_guard(&spec);
        cancel.cancel();
        let err = spawn(spec, guard).expect_err("cancelled spawn");
        assert_eq!(err, SpawnError::Cancelled);
    }

    #[test]
    fn empty_argv_and_zero_or_oversized_timeout_and_oversized_stdin_fail_closed() {
        let echo = if Path::new("/bin/echo").is_file() {
            "/bin/echo"
        } else {
            return;
        };
        assert_eq!(
            ExecSpec::argv(
                Vec::<String>::new(),
                temp_cwd(),
                None::<(String, SecretOrValue)>,
                StdinSpec::Empty,
                None,
                4096,
                CancellationToken::new(),
            )
            .expect_err("empty"),
            SpawnError::EmptyArgv
        );
        assert_eq!(
            ExecSpec::argv(
                [echo],
                temp_cwd(),
                None::<(String, SecretOrValue)>,
                StdinSpec::Empty,
                Some(Duration::ZERO),
                4096,
                CancellationToken::new(),
            )
            .expect_err("timeout"),
            SpawnError::TimeoutInvalid
        );
        assert_eq!(
            ExecSpec::argv(
                [echo],
                temp_cwd(),
                None::<(String, SecretOrValue)>,
                StdinSpec::Empty,
                Some(MAX_PROC_TIMEOUT + Duration::from_secs(1)),
                4096,
                CancellationToken::new(),
            )
            .expect_err("oversized timeout"),
            SpawnError::TimeoutInvalid
        );
        assert_eq!(
            ExecSpec::argv(
                [echo],
                temp_cwd(),
                None::<(String, SecretOrValue)>,
                StdinSpec::Bytes(vec![b'x'; MAX_STDIN_BYTES + 1]),
                None,
                4096,
                CancellationToken::new(),
            )
            .expect_err("stdin"),
            SpawnError::StdinTooLarge
        );
    }

    #[test]
    fn shell_script_canary_is_absent_from_debug_and_errors() {
        let sh = if Path::new("/bin/sh").is_file() {
            "/bin/sh"
        } else {
            return;
        };
        let spec = ExecSpec::shell(
            sh,
            format!("echo {CANARY}"),
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Bytes(CANARY.as_bytes().to_vec()),
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec");
        let debug = format!("{spec:?}");
        assert!(!debug.contains(CANARY));
        let invocation_debug = format!("{:?}", spec.invocation());
        assert!(!invocation_debug.contains(CANARY));
        assert!(invocation_debug.contains("redacted"));
        let stdin_debug = format!("{:?}", spec.stdin());
        assert!(!stdin_debug.contains(CANARY));
        assert!(stdin_debug.contains(&CANARY.len().to_string()));
        assert!(!format!("{:?}", spec.invocation()).contains("echo "));
    }

    #[test]
    fn spawn_error_display_does_not_echo_attacker_input() {
        for err in [
            SpawnError::RelativeExecutable,
            SpawnError::SecretNotMaterialized,
            SpawnError::Nul,
            SpawnError::Traversal,
            SpawnError::LeaseNotBound,
            SpawnError::ShellGrantRequired,
        ] {
            let text = format!("{err:?} {err}");
            assert!(!text.contains(CANARY));
            assert!(!text.contains("/etc/passwd"));
        }
    }

    #[test]
    fn unbound_spec_cannot_spawn() {
        let echo = if Path::new("/bin/echo").is_file() {
            "/bin/echo"
        } else {
            return;
        };
        let spec = ExecSpec::argv(
            [echo],
            temp_cwd(),
            None::<(String, SecretOrValue)>,
            StdinSpec::Empty,
            None,
            4096,
            CancellationToken::new(),
        )
        .expect("spec");
        let bound = spec.clone().bind(test_binding()).expect("bind");
        let guard = lease_guard(&bound);
        let err = spawn(spec, guard).expect_err("unbound");
        assert_eq!(err, SpawnError::LeaseNotBound);
    }
}
