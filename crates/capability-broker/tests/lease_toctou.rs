//! Adversarial lease TOCTOU tests (T-003, T-004, T-005).
//!
//! Mutate argv, cwd, symlink target, URL redirect, agent principal, workspace
//! view, expiry, or remaining uses between approval and execution. Every
//! mismatch must return `policy.lease_invalid` before a canary file write or
//! canary-server connect.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CancellationToken,
    CanonicalAction, CanonicalHostPath, Capability, DEFAULT_LEASE_TTL_SECS, DecisionWithTrace,
    ExecIntent, FilesystemRoot, FilesystemScope, FsIntent, FsNormalizeError, FsResolver,
    LeaseIssuer, LeaseValidator, NetworkIntent, NetworkResolver, NetworkScheme, NetworkScope,
    PolicyDocument, PolicyError, PolicyRevision, PolicySource, PolicyStack, PrincipalRef,
    ProcessScope, Resolver, ResourceDescriptor, evaluate, issue, normalize_exec, normalize_fs,
    normalize_network, request_approval, validate_use,
};
use protocol::{ErrorCode, SessionId, WorkspaceViewId};

const ISSUER_KEY: [u8; 32] = [0x51; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mutation {
    Argv,
    Cwd,
    Symlink,
    Redirect,
    AgentId,
    WorkspaceView,
    Expiry,
    UseReplay,
}

const MUTATIONS: &[(&str, Mutation)] = &[
    ("argv", Mutation::Argv),
    ("cwd", Mutation::Cwd),
    ("symlink", Mutation::Symlink),
    ("redirect", Mutation::Redirect),
    ("agent_id", Mutation::AgentId),
    ("workspace_view", Mutation::WorkspaceView),
    ("expiry", Mutation::Expiry),
    ("use_replay", Mutation::UseReplay),
];

#[test]
fn authorized_action_fires_canary_then_replay_is_lease_invalid() {
    let world = World::new();
    let now = Instant::now();
    let (lease, approved) = world.issue_command(&world.approved_command(), now);
    let canary = Canary::bind(&world.canary_file);
    execute(&world, &lease, &world.agent(), &approved, now, &canary).expect("authorized use");
    assert!(
        canary.tripped(),
        "matching lease must execute the canary side effect"
    );

    canary.reset();
    let err = execute(&world, &lease, &world.agent(), &approved, now, &canary)
        .expect_err("one-shot replay");
    assert_lease_invalid(&err);
    canary.assert_quiet();
}

#[test]
fn table_driven_mutations_return_lease_invalid_before_side_effect() {
    for (name, mutation) in MUTATIONS {
        let world = World::new();
        let canary = Canary::bind(&world.canary_file);
        let err = run_mutation(&world, *mutation, &canary);
        assert_lease_invalid(&err);
        canary.assert_quiet();
        let rendered = format!("{err} {err:?}");
        assert!(
            !rendered.contains(world.canary_file.to_string_lossy().as_ref()),
            "{name}: PolicyError must not echo canary path"
        );
        assert!(
            !rendered.contains(world.view_b.to_string_lossy().as_ref()),
            "{name}: PolicyError must not echo mutated view path"
        );
    }
}

fn run_mutation(world: &World, mutation: Mutation, canary: &Canary) -> PolicyError {
    let now = Instant::now();
    match mutation {
        Mutation::Argv => {
            let (lease, _) = world.issue_command(&world.approved_command(), now);
            let mutated =
                world.normalize_command(["git", "push", "--force"], world.view_a.as_path());
            execute(&world, &lease, &world.agent(), &mutated, now, canary).expect_err("argv")
        }
        Mutation::Cwd => {
            let (lease, _) = world.issue_command(&world.approved_command(), now);
            let mutated = world.normalize_command(["git", "status"], world.view_b.as_path());
            execute(&world, &lease, &world.agent(), &mutated, now, canary).expect_err("cwd")
        }
        Mutation::Symlink => {
            let approved_path = world.view_a.join("src").join("out");
            let (lease, _) = world.issue_fs_write(&approved_path, now);
            std::fs::remove_file(&approved_path).expect("unlink approved symlink");
            std::os::unix::fs::symlink(&world.canary_file, &approved_path)
                .expect("retarget symlink at canary");
            let mutated = world.normalize_fs_write(&approved_path);
            execute(&world, &lease, &world.agent(), &mutated, now, canary).expect_err("symlink")
        }
        Mutation::Redirect => {
            let (lease, approved) = world.issue_net(now);
            let CanonicalAction::Network(approved_net) = &approved else {
                panic!("expected network action");
            };
            let location = format!("http://127.0.0.1:{}/pwn", canary.port());
            let intent = NetworkIntent::redirect(approved_net, location).expect("redirect intent");
            let mutated = CanonicalAction::Network(
                normalize_network(&intent, &world.net, &CancellationToken::new())
                    .expect("redirect normalize"),
            );
            execute(&world, &lease, &world.agent(), &mutated, now, canary).expect_err("redirect")
        }
        Mutation::AgentId => {
            let (lease, approved) = world.issue_command(&world.approved_command(), now);
            execute(&world, &lease, &world.attacker(), &approved, now, canary).expect_err("agent")
        }
        Mutation::WorkspaceView => {
            let approved_path = world.view_a.join("src").join("safe.txt");
            let (lease, _) = world.issue_fs_write(&approved_path, now);
            let other = world.view_b.join("src").join("safe.txt");
            let mutated = world.normalize_fs_write(&other);
            execute(&world, &lease, &world.agent(), &mutated, now, canary)
                .expect_err("workspace view")
        }
        Mutation::Expiry => {
            let (lease, approved) = world.issue_command(&world.approved_command(), now);
            let later = now + Duration::from_secs(u64::from(DEFAULT_LEASE_TTL_SECS));
            execute(&world, &lease, &world.agent(), &approved, later, canary).expect_err("expiry")
        }
        Mutation::UseReplay => {
            let (lease, approved) = world.issue_command(&world.approved_command(), now);
            let first = Canary::bind(&world.tmp.join("first-use.canary"));
            execute(&world, &lease, &world.agent(), &approved, now, &first).expect("first use");
            assert!(first.tripped(), "first use must execute");
            execute(&world, &lease, &world.agent(), &approved, now, canary).expect_err("use replay")
        }
    }
}

fn execute(
    world: &World,
    lease: &capability_broker::CapabilityLease,
    principal: &PrincipalRef,
    actual: &CanonicalAction,
    now: Instant,
    canary: &Canary,
) -> Result<(), PolicyError> {
    let cancel = CancellationToken::new();
    world
        .issuer()
        .verify(
            lease,
            principal,
            lease.session_id(),
            lease.action_hash(),
            now,
        )
        .map_err(PolicyError::from)?;
    let guard = validate_use(&world.validator, lease, &actual, now, &cancel)?;
    let _consumed = guard.consume();
    canary.trip();
    Ok(())
}

fn assert_lease_invalid(err: &PolicyError) {
    assert_eq!(err.error_code(), ErrorCode::PolicyLeaseInvalid);
    assert_eq!(err.error_code().as_str(), "policy.lease_invalid");
}

struct World {
    tmp: PathBuf,
    view_a: PathBuf,
    view_b: PathBuf,
    canary_file: PathBuf,
    policies: PolicyStack,
    validator: LeaseValidator,
    commands: FixedResolver,
    fs: LiveFs,
    net: MapResolver,
}

impl World {
    fn new() -> Self {
        let view_a_id = WorkspaceViewId::new();
        let view_b_id = WorkspaceViewId::new();
        let tmp = fixture_root().join(format!("rapidlm-lease-toctou-{}", SessionId::new()));
        let view_a = tmp.join("views").join(view_a_id.to_string());
        let view_b = tmp.join("views").join(view_b_id.to_string());
        let canary_file = tmp.join("denied.canary");
        std::fs::create_dir_all(view_a.join("src")).expect("view a");
        std::fs::create_dir_all(view_b.join("src")).expect("view b");
        std::fs::write(view_a.join("src").join("safe.txt"), b"approved\n").expect("safe a");
        std::fs::write(view_b.join("src").join("safe.txt"), b"other-view\n").expect("safe b");
        std::os::unix::fs::symlink(
            view_a.join("src").join("safe.txt"),
            view_a.join("src").join("out"),
        )
        .expect("approved symlink");
        assert!(
            view_a.ends_with(view_a_id.to_string()),
            "view A root binds WorkspaceViewId"
        );
        assert!(
            view_b.ends_with(view_b_id.to_string()),
            "view B root binds WorkspaceViewId"
        );

        let policies = ask_stack();
        let validator = LeaseValidator::new(
            LeaseIssuer::from_key(ISSUER_KEY).expect("issuer"),
            PolicyRevision::of_stack(&policies),
        );
        let fs = LiveFs::new(&view_a, &tmp);
        Self {
            tmp,
            view_a,
            view_b,
            canary_file,
            policies,
            validator,
            commands: FixedResolver,
            fs,
            net: MapResolver,
        }
    }

    fn issuer(&self) -> LeaseIssuer {
        LeaseIssuer::from_key(ISSUER_KEY).expect("issuer")
    }

    fn agent(&self) -> PrincipalRef {
        PrincipalRef::parse("agent.writer").expect("agent")
    }

    fn attacker(&self) -> PrincipalRef {
        PrincipalRef::parse("agent.attacker").expect("attacker")
    }

    fn approved_command(&self) -> CanonicalAction {
        self.normalize_command(["git", "status"], self.view_a.as_path())
    }

    fn normalize_command(
        &self,
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: &Path,
    ) -> CanonicalAction {
        let intent = ExecIntent::argv(argv, cwd.to_string_lossy().into_owned(), None::<String>);
        CanonicalAction::Command(
            normalize_exec(&intent, &self.commands, &CancellationToken::new())
                .expect("normalize command"),
        )
    }

    fn normalize_fs_write(&self, path: &Path) -> CanonicalAction {
        let intent = FsIntent::write(FilesystemRoot::Host, path.to_string_lossy().into_owned());
        CanonicalAction::Filesystem(
            normalize_fs(&intent, &self.fs, &CancellationToken::new()).expect("normalize fs"),
        )
    }

    fn issue_command(
        &self,
        action: &CanonicalAction,
        now: Instant,
    ) -> (capability_broker::CapabilityLease, CanonicalAction) {
        let resource = ResourceDescriptor::Process(ProcessScope::new("git").expect("process"));
        let request = ActionRequest::new(
            self.agent(),
            SessionId::new(),
            Capability::ProcExec,
            resource,
            action.clone(),
            "approved git status",
        )
        .expect("request");
        (self.approve_and_issue(&request, now), action.clone())
    }

    fn issue_fs_write(
        &self,
        path: &Path,
        now: Instant,
    ) -> (capability_broker::CapabilityLease, CanonicalAction) {
        let action = self.normalize_fs_write(path);
        let resource = ResourceDescriptor::Filesystem(
            FilesystemScope::host(&path.to_string_lossy()).expect("fs scope"),
        );
        let request = ActionRequest::new(
            self.agent(),
            SessionId::new(),
            Capability::FsWrite,
            resource,
            action.clone(),
            "approved view write",
        )
        .expect("request");
        (self.approve_and_issue(&request, now), action)
    }

    fn issue_net(&self, now: Instant) -> (capability_broker::CapabilityLease, CanonicalAction) {
        let action = CanonicalAction::Network(
            normalize_network(
                &NetworkIntent::connect("https://example.com/health"),
                &self.net,
                &CancellationToken::new(),
            )
            .expect("normalize net"),
        );
        let resource = ResourceDescriptor::Network(
            NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net"),
        );
        let request = ActionRequest::new(
            self.agent(),
            SessionId::new(),
            Capability::NetConnect,
            resource,
            action.clone(),
            "approved origin",
        )
        .expect("request");
        (self.approve_and_issue(&request, now), action)
    }

    fn approve_and_issue(
        &self,
        request: &ActionRequest,
        now: Instant,
    ) -> capability_broker::CapabilityLease {
        let decision = eval(&self.policies, request);
        let approval =
            request_approval(request, &decision, now, &CancellationToken::new()).expect("approval");
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
        assert_eq!(approved.scope().constraints().max_uses(), 1);
        issue(
            &self.issuer(),
            &approved,
            &self.policies,
            now,
            &CancellationToken::new(),
        )
        .expect("issue")
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn fixture_root() -> PathBuf {
    // Prefer a path without intermediate host symlinks (`/var` -> `/private/var`
    // on macOS). Those hops share the walk `seen` set and look like a loop.
    if let Ok(dir) = std::env::var("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir);
    }
    std::env::current_dir()
        .expect("cwd")
        .join("target")
        .join("lease-toctou")
}

fn eval(policies: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
    evaluate(policies, request, &CancellationToken::new()).expect("evaluate")
}

fn ask_stack() -> PolicyStack {
    let user = PolicyDocument::parse_toml(
        r#"
[[rules]]
id = "proc-git"
effect = "allow"
subjects = ["*"]
capability = "proc.exec"
resource = { command_family = "git" }

[[rules]]
id = "fs-host"
effect = "allow"
subjects = ["*"]
capability = "fs.write"
resource = { root = "host" }

[[rules]]
id = "net-ex"
effect = "allow"
subjects = ["*"]
capability = "net.connect"
resource = { scheme = "https", host = "example.com", port = 443 }
"#,
        PolicySource::user("user-policy.toml").expect("user"),
        &CancellationToken::new(),
    )
    .expect("user policy");
    let project = PolicyDocument::parse_toml(
        r#"
[[rules]]
id = "proc-ask"
effect = "ask"
subjects = ["*"]
capability = "proc.exec"

[[rules]]
id = "fs-ask"
effect = "ask"
subjects = ["*"]
capability = "fs.write"

[[rules]]
id = "net-ask"
effect = "ask"
subjects = ["*"]
capability = "net.connect"
"#,
        PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
        &CancellationToken::new(),
    )
    .expect("project policy");
    PolicyStack::new([user, project]).expect("stack")
}

struct FixedResolver;

impl Resolver for FixedResolver {
    fn resolve_cwd(
        &self,
        requested: &str,
    ) -> Result<CanonicalHostPath, capability_broker::CommandNormalizeError> {
        CanonicalHostPath::from_resolved(requested)
    }

    fn resolve_executable(
        &self,
        requested: &str,
        _cwd: &CanonicalHostPath,
    ) -> Result<CanonicalHostPath, capability_broker::CommandNormalizeError> {
        let resolved = match requested {
            "git" => "/usr/bin/git",
            "echo" => "/usr/bin/echo",
            other if other.starts_with('/') => other,
            _ => return Err(capability_broker::CommandNormalizeError::UnresolvedExecutable),
        };
        CanonicalHostPath::from_resolved(resolved)
    }
}

struct LiveFs {
    repo_root: CanonicalHostPath,
    host_base: CanonicalHostPath,
}

impl LiveFs {
    fn new(repo_root: &Path, host_base: &Path) -> Self {
        Self {
            repo_root: CanonicalHostPath::from_resolved(&repo_root.to_string_lossy())
                .expect("repo root"),
            host_base: CanonicalHostPath::from_resolved(&host_base.to_string_lossy())
                .expect("host base"),
        }
    }
}

impl FsResolver for LiveFs {
    fn repo_root(&self) -> &CanonicalHostPath {
        &self.repo_root
    }

    fn host_base(&self) -> &CanonicalHostPath {
        &self.host_base
    }

    fn exists(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError> {
        match std::fs::symlink_metadata(path.as_str()) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(_) => Err(FsNormalizeError::UnresolvedPath),
        }
    }

    fn is_dir(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError> {
        match std::fs::symlink_metadata(path.as_str()) {
            Ok(meta) => Ok(meta.is_dir()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(_) => Err(FsNormalizeError::UnresolvedPath),
        }
    }

    fn read_link(&self, path: &CanonicalHostPath) -> Result<Option<String>, FsNormalizeError> {
        let meta = match std::fs::symlink_metadata(path.as_str()) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(FsNormalizeError::UnresolvedPath),
        };
        if !meta.file_type().is_symlink() {
            return Ok(None);
        }
        let target =
            std::fs::read_link(path.as_str()).map_err(|_| FsNormalizeError::UnresolvedPath)?;
        let text = target
            .to_str()
            .ok_or(FsNormalizeError::UnresolvedPath)?
            .to_owned();
        Ok(Some(text))
    }
}

struct MapResolver;

impl NetworkResolver for MapResolver {
    fn resolve(
        &self,
        host: &capability_broker::Hostname,
    ) -> Result<Vec<std::net::IpAddr>, capability_broker::NetworkNormalizeError> {
        match host.as_str() {
            "example.com" => Ok(vec!["93.184.216.34".parse().expect("ip")]),
            _ => Err(capability_broker::NetworkNormalizeError::UnresolvedHost),
        }
    }
}

struct Canary {
    file: PathBuf,
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Canary {
    fn bind(file: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("canary listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hits_thread = Arc::clone(&hits);
        let stop_thread = Arc::clone(&stop);
        let join = thread::spawn(move || {
            loop {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok(_) => {
                        hits_thread.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            file: file.to_path_buf(),
            addr,
            hits,
            stop,
            join: Some(join),
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn trip(&self) {
        std::fs::write(&self.file, b"EXECUTED\n").expect("canary file");
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        for _ in 0..50 {
            if self.hits.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn tripped(&self) -> bool {
        self.file.exists() || self.hits.load(Ordering::SeqCst) > 0
    }

    fn reset(&self) {
        let _ = std::fs::remove_file(&self.file);
        self.hits.store(0, Ordering::SeqCst);
    }

    fn assert_quiet(&self) {
        assert!(
            !self.file.exists(),
            "denied execution must not write canary file {}",
            self.file.display()
        );
        assert_eq!(
            self.hits.load(Ordering::SeqCst),
            0,
            "denied execution must not connect to canary server"
        );
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
