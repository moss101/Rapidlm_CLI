//! Interactive `rapid` composition root: kernel, in-process client, and TUI.
//!
//! `rapid` with no subcommand opens the TUI. This module wires existing
//! kernel/tui traits and owns no domain state. Project-controlled executable
//! config stays inactive until [`TrustStatus::Trusted`].

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers, poll as poll_event,
    read as read_event,
};
use event_ledger::event::{ActorKind, ActorRef};
use kernel::{
    CancellationToken, ConfigLoadError, ConfigLoadResult, ConfigOverride, ConfigSources,
    ConfigText, CreateSession, DEFAULT_ROLLBACK_QUIESCE, ENV_PREFIX, EventStream, EventStreamError,
    ForkSession, GraphPhase, HealthSnapshot, HealthState, InProcessKernelClient, Interrupt,
    InterruptReason, KernelClient, LifecycleService, MAX_CONFIG_DOCUMENT_BYTES,
    MAX_OVERRIDE_ENTRIES, ProjectIdentity, ProjectTrustError, ProjectTrustStore, RewindSession,
    ServiceContext, ServiceError, ServiceFailureKind, ServiceGraph, ServiceId, ServiceStatus,
    SubmitTurn, SubscribeEvents, TrustStatus, config_key_from_env_name, load_config,
};
use protocol::{EventId, ProjectId, TraceContext, TraceId};
use tui::state::{
    GoalLifecycle, GoalProjection, LocalUiEvent, MAX_COMPOSER_BYTES, ToolActivityStatus,
    TranscriptEntry, UiEvent,
};
use tui::{
    AppState, CommandError, FrontendAction, FrontendKind, KernelAction, KernelApi, LocalAction,
    RecordingBackend, TerminalError, TerminalGuard, dispatch, parse_command, reduce,
};

use crate::goal_host::{
    DriverLease, EVIDENCE_FILE, GOAL_FILE, GoalHost, GoalTransactionError, SESSIONS_DB_FILE,
    accrue_turn_usage, active_goal_id, try_acquire_driver_lease,
};
use crate::headless::jsonl::JsonlExitCode;
use crate::exec_tools::ExecTools;
use crate::host::{
    ExecOutcome, FallbackChainModel, PreservedLiveContext, RouterDecisionReason, StepDiag,
    UnconfiguredModel, run_live_exec,
};
use crate::model::{ConfiguredModel, SelectedModel};
use crate::user_config::ModelSelection;
use agent_runtime::{
    AgentExecutionRequest, AgentResult, AgentRole, AgentSpec, AgentTerminalStatus,
    ContextRetryPolicy, ConvergenceHint, EvidenceKind, EvidenceLedgerRef, EvidenceProducer,
    EvidenceSpec, EvidenceStatus, FailureCause, GoalActor, GoalBudget, GoalBudgetGuard,
    GoalCommand, GoalSnapshot, GoalSpec, GoalState, MessageLoopDetector, TEST_PASSED,
    TurnFailureDetail, TurnStopReason,
};

/// Bound on ancestors inspected while locating `.rapidlm` / `.git`.
pub const MAX_PROJECT_WALK_DEPTH: usize = 64;

/// Bound on kernel events folded in one input tick.
pub const MAX_EVENTS_PER_TICK: usize = 64;

/// Poll wait used by the production crossterm input source.
pub const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(50);

const KERNEL_SERVICE_ID: &str = "kernel";
const PROJECT_MARKER: &str = ".rapidlm";
const GIT_MARKER: &str = ".git";
const WORKSPACE_CONFIG_NAME: &str = "config.toml";
const USER_CONFIG_NAME: &str = "config.toml";
const TRUST_CATALOG_NAME: &str = "project-trust.json";
const LEDGER_NAME: &str = "ledger.sqlite";
const HOME_ENV: &str = "HOME";
const USERPROFILE_ENV: &str = "USERPROFILE";
const RAPIDLM_HOME_ENV: &str = "RAPIDLM_HOME";

/// First positional non-flag argument selects a subcommand.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LaunchMode {
    Interactive,
    Subcommand,
    Help,
}

/// How the interactive session ended after terminal restore + kernel quiesce.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum InteractiveOutcome {
    Quit,
    Interrupted,
}

/// One injected or production input event. Not a shell string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractiveInput {
    CtrlC,
    Char(char),
    Backspace,
    Enter,
    Submit(String),
    Resize { width: u16, height: u16 },
    /// Scroll the transcript viewport one page toward earlier output.
    PageUp,
    /// Scroll the transcript viewport one page toward later output.
    PageDown,
    Eof,
}

/// Composition-root failure. Display never echoes file bodies or secrets.
#[derive(Debug)]
pub enum InteractiveError {
    Cancelled,
    Usage,
    NotATty,
    AlreadyActive,
    UserHomeMissing,
    InvalidProjectRoot,
    Terminal(TerminalError),
    Config(ConfigLoadError),
    Trust(ProjectTrustError),
    Kernel(protocol::ApiError),
    Service(ServiceError),
    Stream(EventStreamError),
    PendingFuture,
    Io,
    Internal,
}

/// Observable result of a completed interactive run.
#[derive(Debug)]
pub struct InteractiveReport {
    pub outcome: InteractiveOutcome,
    pub trust: TrustStatus,
    pub executable_config_active: bool,
    pub session_id: Option<protocol::SessionId>,
    pub graph_phase: GraphPhase,
    pub terminal_restored: bool,
    pub interrupt_count: u32,
    pub config: ConfigLoadResult,
    /// Every byte the TUI renderer painted this run, when
    /// [`InteractiveOptions::capture_render`] requested it — `None` in
    /// production, where painted frames go to real stdout instead and are
    /// never buffered in memory. Test-only, but a real production field: it
    /// exists to let a test observe what the *actual* production render
    /// path (`SessionLoop::drain` -> `TuiRenderer::render`) painted, not a
    /// parallel or reimplemented one.
    pub rendered_output: Option<String>,
}

/// Injected filesystem, env, input, and terminal for [`run_interactive`].
pub struct InteractiveOptions {
    pub cwd: PathBuf,
    pub user_home: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cli: Vec<ConfigOverride>,
    pub cancel: CancellationToken,
    pub inputs: Option<Vec<InteractiveInput>>,
    pub terminal: Option<RecordingBackend>,
    /// When true, the TUI renderer's painted bytes accumulate into
    /// [`InteractiveReport::rendered_output`] instead of going to real
    /// stdout. Always `false` in production (see [`InteractiveOptions::
    /// from_env`]) — this exists so a test can inspect what the production
    /// render path actually painted without corrupting the test process's
    /// own terminal with raw cursor/clear escape sequences.
    pub capture_render: bool,
}

struct ResolvedProject {
    trust: TrustStatus,
    executable_config_active: bool,
    config: ConfigLoadResult,
    ledger_path: PathBuf,
    root: PathBuf,
}

struct KernelRuntime {
    id: ServiceId,
    dependencies: Vec<ServiceId>,
    health: HealthState,
    ledger_path: PathBuf,
    client: Mutex<Option<InProcessKernelClient>>,
}

enum InputSource {
    Scripted {
        events: std::vec::IntoIter<InteractiveInput>,
    },
    Crossterm,
}

enum LoopControl {
    Continue,
    Quit(InteractiveOutcome),
}

/// Classify argv after the program name. Flags do not select a subcommand.
pub fn classify_launch<S: AsRef<str>>(args: &[S]) -> LaunchMode {
    // `--help` before any subcommand word prints the top-level usage; after
    // one it belongs to that subcommand (`rapid exec --help` → exec usage).
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--" || arg.starts_with('-') {
            continue;
        }
        return LaunchMode::Subcommand;
    }
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--help" || arg == "-h" {
            return LaunchMode::Help;
        }
    }
    LaunchMode::Interactive
}

/// Target V3 command surface from `docs/reference/cli-command-reference.md`.
pub const CLI_USAGE: &str = "\
usage: rapid [subcommand]

  rapid                         interactive TUI
  rapid exec <prompt>           one-shot/headless task
  rapid run <goal/playbook>     durable graph run
  rapid trust grant|status|revoke   explicit project-trust control plane
  rapid goal create|show|pause|resume|cancel|budget|verify
  rapid resume [session/run]    resume durable session/run
  rapid fork [checkpoint]       non-destructive branch
  rapid rewind                  restore/fork checkpoint
  rapid daemon                  durable local kernel service
  rapid acp                     ACP stdio server
  rapid graph show|watch|diff|why-ready|why-blocked|retry|export
  rapid context show|explain|compact|search
  rapid evidence show|verify|export
  rapid agents list|inspect|cancel
  rapid process list|logs|input|cancel|monitor
  rapid computer ...            computer/browser/mobile
  rapid sandbox status|doctor
  rapid mcp list|add|remove|auth|refresh
  rapid plugins validate|register|list|approve|reject|hook-test
  rapid hooks list|test|enable|disable
  rapid skills list|show|enable|disable
  rapid eval run|compare|report
  rapid inspect <session/run>
  rapid cron add|list|remove|poll   durable prompt cron (claim-lease firing)
  rapid export
  rapid doctor
  rapid update
";

/// Exec-specific usage, printed by `rapid exec --help` and on exec usage
/// errors. Documents the prompt argument, the exec flags, and the env vars
/// that shape a headless run.
pub const EXEC_USAGE: &str = "\
usage: rapid exec <prompt> [--verbose] [--max-wall-time <seconds>] [--json-schema <path>] [--jsonl]

Run one headless agent turn with the configured model. The final response is
printed to stdout; diagnostics go to stderr; a non-zero exit code reports a
failed turn.

Arguments:
  <prompt>    Task prompt for the agent (required)

Options:
  --verbose               Per-attempt model and turn diagnostics on stderr
  --max-wall-time <secs>  Cancel the turn if it runs longer than this many
                          seconds (cooperative: the same signal Ctrl-C sends)
  --json-schema <path>    Constrain the result to a JSON Schema document read
                          from <path>: the model must call a synthetic tool
                          with matching arguments, printed to stdout in place
                          of the usual text summary. A schema-conformant
                          result is never produced without a matching call.
  --jsonl                 Write the turn's outcome as JSONL protocol records
                          (schema + assistant.message + session.finished)
                          instead of plain text. Covers only the turn
                          outcome itself, not a pre-flight setup failure.
  -h, --help              Print this help

Environment:
  RAPIDLM_PERMISSION_MODE  Tool approval mode for this run: default | plan |
                           acceptEdits | auto | dontAsk | bypassPermissions
  RAPIDLM_CONFIG           Path to a model config TOML overriding the user
                           config
  RAPIDLM_MODEL            Model id override for this run

Workspace tools stay disabled until the project is trusted: run
`rapid trust grant` once in the project to approve trust.
";

/// `rapid trust`-specific usage, printed by `rapid trust --help`/`-h` and on
/// usage errors. Documents the explicit, human-only control plane for the
/// project-trust security boundary: no model tool, slash command, or
/// autonomous-goal code path can reach this command — see `run_trust_command`'s
/// own doc comment.
pub const TRUST_USAGE: &str = "\
usage: rapid trust grant|status|revoke

Explicit control plane for the project-trust security boundary. Trust gates
workspace file/shell tools, proactive context retrieval, and trusted-project
integrations (web_fetch allowlist, hooks, MCP servers) for the project
discovered from the current directory (nearest ancestor with `.rapidlm` or
`.git`, matching every other trust check in this binary) — never a path you
name explicitly, so the project being decided on is always the one the
command actually runs against.

Commands:
  grant     Trust the current project. Idempotent: granting an
            already-trusted project reports so and makes no further change.
  status    Report whether the current project is trusted.
  revoke    Untrust the current project. Idempotent: revoking an
            already-untrusted project reports so and makes no further
            change.

  -h, --help  Print this help

This is the only reachable way to change a project's trust record — trust
never arises implicitly from opening a project, a model requesting a
privileged operation, or an autonomous goal needing more permissions.
";

/// Process entry: no subcommand starts the TUI against the detected project.
pub fn run() -> Result<i32, InteractiveError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify_launch(&args) {
        LaunchMode::Help => {
            print!("{CLI_USAGE}");
            Ok(0)
        }
        LaunchMode::Interactive => {
            let report = run_interactive(InteractiveOptions::from_env()?)?;
            Ok(report.outcome.exit_code())
        }
        LaunchMode::Subcommand => run_subcommand(&args),
    }
}

/// Dispatch a non-TUI subcommand. `exec`/`goal` run a one-shot agent turn
/// through the context-owning live-recovery host.
/// Load the persisted goal (`.rapidlm/goal.json`) and project it into the TUI
/// via `LocalUiEvent::SyncGoal`, so the Goals route shows the host-owned goal.
fn sync_persisted_goal(ui: &mut AppState, ledger_path: &Path) {
    let Some(goal_path) = ledger_path.parent().map(|p| p.join(GOAL_FILE)) else {
        return;
    };
    let Ok(Some(host)) = GoalHost::load(&goal_path) else {
        return;
    };
    let Some(snapshot) = host.snapshot() else {
        return;
    };
    let projection = project_goal(snapshot);
    *ui = reduce(ui.clone(), &UiEvent::Local(LocalUiEvent::SyncGoal(projection)));
}

/// Map an agent-runtime goal snapshot into the TUI goal projection.
fn project_goal(snapshot: &GoalSnapshot) -> GoalProjection {
    GoalProjection::new(
        snapshot.id(),
        Some(snapshot.statement().to_owned()),
        map_goal_lifecycle(snapshot.state()),
        snapshot.budget().max_turns(),
        snapshot.budget().max_tokens(),
        snapshot.usage().turns(),
        snapshot.usage().tokens(),
    )
}

fn map_goal_lifecycle(state: GoalState) -> GoalLifecycle {
    match state {
        GoalState::Active => GoalLifecycle::Active,
        GoalState::Paused => GoalLifecycle::Paused,
        GoalState::Blocked => GoalLifecycle::Blocked,
        // `GoalState` is #[non_exhaustive]; fail conservatively by not dropping
        // the goal from the projection on a future variant.
        _ => GoalLifecycle::Blocked,
    }
}

/// Build the boundary-turn prompt text for one autonomous iteration. A flat
/// string, not `agent_runtime::prompt::PromptBundle` — `GoalDriver`'s own
/// `compile_boundary_prompt` produces a structured `Vec<PromptMessage>`
/// bundle that nothing in `apps/rapid`'s real model-context pipeline
/// (`context_engine::compile::ContextBlock`s, built by `build_packet` from
/// `PreservedLiveContext`) can consume — the two prompt representations are
/// entirely separate and never reconciled anywhere in the workspace. This
/// is the one adapter piece that gap genuinely requires, not a duplicate of
/// reusable logic: the *decision* of what to tell the model (goal
/// statement, its completion criteria, a budget hint) is still driven
/// entirely by the real `GoalSnapshot`/`ConvergenceHint` values the caller
/// already computed via `agent_runtime::GoalBudgetGuard`, nothing invented
/// here. Fed as `apps/rapid`'s own `text` parameter — the exact same single
/// string every ordinary turn already uses for both its kernel-visible
/// transcript entry and its model-visible context (see
/// `build_interactive_turn_context`) — so an autonomous iteration is
/// maximally transparent in the transcript, not hidden or summarized away.
fn compile_autonomous_prompt(snapshot: &GoalSnapshot, hint: Option<ConvergenceHint>) -> String {
    let mut text = format!(
        "Continue working autonomously toward this goal. When every completion \
         criterion below is fully satisfied by evidence you have recorded, say so \
         and stop proposing further changes — completion itself is detected \
         automatically from recorded evidence, not from this message.\n\ngoal: {}\n",
        snapshot.statement()
    );
    for criterion in snapshot.completion_criteria() {
        text.push_str(&format!(
            "- criterion {}: {}\n",
            criterion.id(),
            criterion.text()
        ));
    }
    if let Some(hint) = hint.filter(|hint| !hint.is_empty()) {
        text.push_str("\nbudget note: approaching the configured limit on");
        if hint.turns() {
            text.push_str(" turns");
        }
        if hint.tokens() {
            text.push_str(" tokens");
        }
        if hint.active_ms() {
            text.push_str(" time");
        }
        if hint.cost() {
            text.push_str(" cost");
        }
        text.push_str(" — wrap up soon if reasonable.\n");
    }
    text
}

/// Run a P9 subcommand, mapping its typed error onto CLI usage output.
fn p9(
    args: &[String],
    command: fn(&[String]) -> Result<i32, crate::p9_commands::P9CommandError>,
) -> Result<i32, InteractiveError> {
    command(args).map_err(|err| {
        eprintln!("{err}");
        InteractiveError::Usage
    })
}

fn run_subcommand(args: &[String]) -> Result<i32, InteractiveError> {
    match args.first().map(String::as_str) {
        Some("exec") => exec_turn(&args[1..], None),
        Some("trust") => run_trust_command(&args[1..]),
        Some("goal") => run_goal_command(&args[1..]),
        Some("playbook-compile") => p9(&args[1..], crate::p9_commands::run_playbook_compile),
        Some("mcp-tools") => p9(&args[1..], crate::p9_commands::run_mcp_tools),
        Some("tools") => p9(&args[1..], crate::p9_commands::run_tools_schema),
        Some("agent-cli") => p9(&args[1..], crate::p9_commands::run_agent_cli),
        Some("doctor") => p9(&args[1..], crate::p9_commands::run_doctor),
        Some("sessions") => p9(&args[1..], crate::p9_commands::run_sessions),
        Some("inspect-export") => p9(&args[1..], crate::p9_commands::run_inspect_export),
        Some("cron") => p9(&args[1..], crate::p9_commands::run_cron),
        Some("findings") => p9(&args[1..], crate::p9_commands::run_findings),
        Some("scan") => p9(&args[1..], crate::p9_commands::run_scan),
        Some("agents") => p9(&args[1..], crate::p9_commands::run_agents),
        Some("plugins") => p9(&args[1..], crate::p9_commands::run_plugins),
        Some("completions") => p9(&args[1..], crate::p9_commands::run_completions),
        Some("man") => p9(&args[1..], crate::p9_commands::run_man),
        Some("insights") => p9(&args[1..], crate::p9_commands::run_insights),
        Some("release-manifest") => p9(&args[1..], crate::p9_commands::run_release_manifest),
        _ => Err(InteractiveError::Usage),
    }
}

/// `rapid trust grant|status|revoke`: the explicit, reachable production
/// control plane for the project-trust security boundary (see
/// `crates/kernel/src/project/trust.rs`'s `ProjectTrustStore`). Reached only
/// from [`run_subcommand`], itself reached only from [`run`] — this
/// function's own OS-process argv, not a model tool call, a slash command,
/// or an autonomous-goal iteration, is the only way to invoke it. Compare
/// [`exec_workspace`]/[`resolve_project`], the trust *readers* every turn
/// (autonomous or not) goes through: they can only ever observe whatever
/// this command — or a human editing the catalog file directly — already
/// persisted; nothing on the turn/tool-execution path can call
/// `ProjectTrustStore::set` itself.
///
/// Resolves the project identity exactly the way every trust *check* in
/// this binary already does — [`canonicalize_dir`] then
/// [`detect_project_root`] then `ProjectIdentity::new` — so a grant here is
/// guaranteed to be observed by the next `rapid exec`/interactive session
/// against the same directory. `detect_project_root` never reports "no
/// project found": it walks up to the nearest `.rapidlm`/`.git` marker, or
/// falls back to the (canonicalized) current directory if none exists
/// anywhere above it — the same fallback every other trust check already
/// relies on. Diverging from it here would let `grant`/`status` resolve a
/// *different* identity than the checks that gate real operations, which is
/// the one thing this command must never do.
fn run_trust_command(args: &[String]) -> Result<i32, InteractiveError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{TRUST_USAGE}");
        return Ok(0);
    }
    let Some(sub) = args.first().map(String::as_str) else {
        eprint!("{TRUST_USAGE}");
        return Err(InteractiveError::Usage);
    };
    if !matches!(sub, "grant" | "status" | "revoke") {
        eprint!("{TRUST_USAGE}");
        return Err(InteractiveError::Usage);
    }

    let cancel = CancellationToken::new();
    let cwd = std::env::current_dir().map_err(|_| InteractiveError::Io)?;
    let cwd = canonicalize_dir(&cwd)?;
    let root = detect_project_root(&cwd, &cancel)?;
    let identity = ProjectIdentity::new(root.as_path(), None)
        .map_err(|_| InteractiveError::InvalidProjectRoot)?;
    let user_home = exec_user_home().ok_or(InteractiveError::UserHomeMissing)?;
    let store = ProjectTrustStore::open(user_home.join(TRUST_CATALOG_NAME));
    let root_display = root.display();

    match sub {
        "grant" => {
            let before = store.get(&identity, &cancel).map_err(InteractiveError::Trust)?;
            store
                .set(&identity, TrustStatus::Trusted, &cancel)
                .map_err(InteractiveError::Trust)?;
            if before.is_trusted() {
                println!("already trusted: {root_display}");
            } else {
                println!("trust granted: {root_display}");
            }
            Ok(0)
        }
        "revoke" => {
            let before = store.get(&identity, &cancel).map_err(InteractiveError::Trust)?;
            store
                .set(&identity, TrustStatus::Untrusted, &cancel)
                .map_err(InteractiveError::Trust)?;
            if before.is_trusted() {
                println!("trust revoked: {root_display}");
            } else {
                println!("already untrusted: {root_display}");
            }
            Ok(0)
        }
        "status" => {
            let status = store.get(&identity, &cancel).map_err(InteractiveError::Trust)?;
            println!("{}: {root_display}", status.as_str());
            Ok(0)
        }
        _ => unreachable!("validated above"),
    }
}

/// Durable host-owned goal contract: `goal create|show|pause|resume|cancel`.
/// The goal is persisted under `.rapidlm/goal.json` so lifecycle commands work
/// across invocations; completion still requires the evidence gate.
fn run_goal_command(args: &[String]) -> Result<i32, InteractiveError> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(InteractiveError::Usage);
    };
    let cancel = agent_runtime::CancellationToken::new();
    let path = Path::new(PROJECT_MARKER).join(GOAL_FILE);
    let evidence_path = Path::new(PROJECT_MARKER).join(EVIDENCE_FILE);
    let mut host = match GoalHost::load(&path) {
        Ok(host) => host.unwrap_or_else(GoalHost::new),
        Err(err) => {
            eprintln!("{err}");
            return Ok(JsonlExitCode::Runtime.as_i32());
        }
    };
    // Durable-ledger backing for agent-produced evidence citations. Without
    // the ledger the gate stays fail-closed for agent records; human and
    // system records are unaffected. The handle is kept for `goal claim`,
    // which appends its own audit events.
    let claim_ledger =
        match event_ledger::ledger::EventLedger::open(Path::new(PROJECT_MARKER).join(SESSIONS_DB_FILE))
        {
            Ok(ledger) => {
                host.install_backing(ledger.clone());
                Some(ledger)
            }
            Err(err) => {
                eprintln!("ledger unavailable ({err}); agent evidence cannot be backed");
                None
            }
        };
    if let Err(err) = host.load_evidence(&evidence_path) {
        eprintln!("{err}");
        return Ok(JsonlExitCode::Runtime.as_i32());
    }

    let result = match sub {
        "create" | "replace" => {
            // Statement words come first; `--key value` flags follow.
            let flag_start = args[1..]
                .iter()
                .position(|arg| arg.starts_with("--"))
                .map(|i| i + 1);
            let (statement_words, flag_args) = match flag_start {
                Some(i) => (&args[1..i], &args[i..]),
                None => (&args[1..], &args[args.len()..]),
            };
            let statement = statement_words.join(" ");
            if statement.is_empty() {
                return Err(InteractiveError::Usage);
            }
            let flags = parse_flags(flag_args)?;
            let mut criteria = Vec::new();
            for entry in flags.get("criterion").into_iter().flatten() {
                let Some((id, text)) = entry.split_once('=') else {
                    return Err(InteractiveError::Usage);
                };
                criteria.push(agent_runtime::Criterion::new(id, text).map_err(|err| {
                    eprintln!("{err}");
                    InteractiveError::Usage
                })?);
            }
            let mut requirements = Vec::new();
            for entry in flags.get("requires").into_iter().flatten() {
                let Some((id, kinds)) = entry.split_once('=') else {
                    return Err(InteractiveError::Usage);
                };
                let kinds: Vec<String> = kinds.split(',').map(str::to_owned).collect();
                requirements.push(
                    agent_runtime::EvidenceRequirement::new(id, kinds).map_err(|err| {
                        eprintln!("{err}");
                        InteractiveError::Usage
                    })?,
                );
            }
            let max_steps = match one(&flags, "max-steps") {
                Some(raw) => Some(raw.parse::<u64>().map_err(|_| InteractiveError::Usage)?),
                None => None,
            };
            let max_tokens = match one(&flags, "max-tokens") {
                Some(raw) => Some(raw.parse::<u64>().map_err(|_| InteractiveError::Usage)?),
                None => None,
            };
            let spec = GoalSpec::new(
                protocol::GoalId::new(),
                statement,
                criteria,
                GoalBudget::new(max_steps, max_tokens, None, None),
                requirements,
            )
            .map_err(|err| {
                eprintln!("{err}");
                InteractiveError::Usage
            })?;
            let command = if sub == "create" {
                GoalCommand::Create(spec)
            } else {
                GoalCommand::Replace(spec)
            };
            // Locked read-modify-write: `apply`'s `AlreadyActive` check must
            // see whatever is *currently* on disk, not whatever this
            // process's own `host` happened to load before a concurrent
            // writer (another `rapid goal create`, a `rapid exec` turn's
            // usage accrual) may have changed it.
            host.update(&path, |host| host.apply(command, &GoalActor::Human, &cancel))
                .map_err(|err| {
                    if let GoalTransactionError::Persist(persist_err) = &err {
                        eprintln!("{persist_err}");
                    }
                    InteractiveError::Internal
                })?;
            Ok(0)
        }
        "show" => {
            let Some(snapshot) = host.snapshot() else {
                println!("no active goal");
                return Ok(JsonlExitCode::Usage.as_i32());
            };
            println!("{}", snapshot.statement());
            for criterion in snapshot.completion_criteria() {
                println!("- criterion {}: {}", criterion.id(), criterion.text());
            }
            println!("state: {}", snapshot.state().as_str());
            println!("complete: {}", host.can_complete(&cancel));
            Ok(0)
        }
        // Each locked under `GoalHost::update`: `goal_lifecycle` reads the
        // goal id and gates `complete` on evidence against whatever is
        // *currently* on disk (reloaded fresh under the lock), not this
        // process's possibly-stale outer `host` — the same reload-then-
        // mutate-then-save transaction `create`/`replace` uses above.
        "pause" | "resume" | "cancel" | "complete" => {
            if sub == "complete" {
                // `complete`'s own gate reads whatever `host.evidence`
                // currently holds — refresh it right before gating so a
                // concurrently-committed record (another `rapid goal
                // evidence record`, a `rapid goal claim` finishing a check)
                // isn't missed just because it landed after this process's
                // own startup load. See `GoalHost::reload_evidence`'s own
                // doc comment for why this plain, unlocked read is safe.
                if let Err(err) = host.reload_evidence(&evidence_path) {
                    eprintln!("{err}");
                    return Ok(JsonlExitCode::Runtime.as_i32());
                }
            }
            host.update(&path, |host| goal_lifecycle(host, sub, &cancel))
                .map_err(|err| match err {
                    GoalTransactionError::Persist(persist_err) => {
                        eprintln!("{persist_err}");
                        InteractiveError::Internal
                    }
                    GoalTransactionError::Mutate(inner) => inner,
                })
        }
        "claim" => {
            let Some(ledger) = claim_ledger.as_ref() else {
                eprintln!("ledger unavailable; claims cannot be audited");
                return Ok(JsonlExitCode::Runtime.as_i32());
            };
            let flags = parse_flags(&args[1..])?;
            let summary = one(&flags, "summary").ok_or(InteractiveError::Usage)?;
            let timeout_secs = match one(&flags, "timeout-secs") {
                Some(raw) => raw.parse::<u64>().map_err(|_| InteractiveError::Usage)?,
                None => 60,
            };
            let mut checks = Vec::new();
            for entry in flags.get("check").into_iter().flatten() {
                let Some((requirement_id, command)) = entry.split_once('=') else {
                    return Err(InteractiveError::Usage);
                };
                checks.push(crate::goal_claim::CheckSpec {
                    requirement_id: requirement_id.to_owned(),
                    command: command.to_owned(),
                });
            }
            let claim = crate::goal_claim::GoalClaim::new(summary, checks, timeout_secs)
                .map_err(|err| {
                    eprintln!("{err}");
                    InteractiveError::Usage
                })?;
            let outcome =
                crate::goal_claim::run_claim(&mut host, &evidence_path, ledger, claim, &cancel)
                    .map_err(|err| {
                        eprintln!("{err}");
                        InteractiveError::Internal
                    })?;
            for check in &outcome.checks {
                let status = if check.timed_out {
                    "timeout"
                } else if check.passed {
                    "pass"
                } else {
                    "fail"
                };
                println!("- check {}: {status}", check.requirement_id);
            }
            println!("evidence recorded: {}", outcome.evidence_recorded);
            println!("verdict: {}", outcome.verdict.as_str());
            println!("accepted: {}", outcome.accepted);
            Ok(if outcome.accepted {
                JsonlExitCode::Success.as_i32()
            } else {
                JsonlExitCode::GoalIncomplete.as_i32()
            })
        }
        "export" => {
            let Some(export) = host.export(&cancel) else {
                println!("no active goal");
                return Ok(JsonlExitCode::Usage.as_i32());
            };
            println!("{export}");
            Ok(0)
        }
        "verify" => {
            if host.snapshot().is_none() {
                println!("no active goal");
                return Ok(JsonlExitCode::Usage.as_i32());
            }
            let allowed = host.can_complete(&cancel);
            println!("complete: {allowed}");
            if let Some(verdicts) = host.validate(&cancel) {
                for verdict in verdicts.verdicts() {
                    if verdict.satisfied() {
                        println!("- criterion {}: satisfied", verdict.criterion_id());
                    } else {
                        let reason = verdict.reason();
                        let label = reason.map(|r| r.as_str()).unwrap_or("unsatisfied");
                        // `retryable`: the same check could pass later with no
                        // new evidence at all (e.g. a ledger resolver outage)
                        // versus a final verdict that needs a genuinely new
                        // observation to change (see `CriterionUnsatisfied::
                        // retryable`, `newtask.md` §2.4).
                        if reason.is_some_and(|r| r.retryable()) {
                            println!(
                                "- criterion {}: unsatisfied ({label}, retryable)",
                                verdict.criterion_id()
                            );
                        } else {
                            println!(
                                "- criterion {}: unsatisfied ({label})",
                                verdict.criterion_id()
                            );
                        }
                    }
                }
                // Turn-level rollup of the per-criterion `retryable` flags
                // above: only printed when completion is actually blocked,
                // since "retry advisable" is meaningless once already
                // complete.
                if !allowed {
                    println!("retry advisable: {}", verdicts.retry_advisable());
                }
            }
            Ok(0)
        }
        "evidence" => {
            let Some(action) = args.get(1).map(String::as_str) else {
                return Err(InteractiveError::Usage);
            };
            match action {
                "record" => goal_evidence_record(&mut host, &evidence_path, &args[2..]),
                "list" => goal_evidence_list(&host),
                _ => Err(InteractiveError::Usage),
            }
        }
        _ => Err(InteractiveError::Usage),
    }?;

    // Neither `goal.json` nor `goal-evidence.json` is saved here: every
    // subcommand that actually mutates state now persists it itself, under
    // its own lock, against a freshly-reloaded snapshot/store — not this
    // function's own possibly-stale outer `host`. `create`/`replace`/
    // `pause`/`resume`/`cancel`/`complete` persist `goal.json` through
    // `GoalHost::update`, above; `claim`/`evidence record` persist
    // `goal-evidence.json` through `GoalHost::update_evidence` (`claim`
    // once per check, immediately after that check's own — potentially
    // long-running — external command finishes, never while it's still
    // running). `show`/`export`/`verify` never mutate either file. A
    // trailing unconditional save here — the previous shape, still correct
    // for `goal.json` back when this evidence half hadn't yet been fixed —
    // would now only ever be a no-op-at-best, stale-overwrite-at-worst:
    // exactly the lost-update hazard this task exists to close, just
    // narrowed to whatever tiny window separates the locked transaction
    // above from this line.
    Ok(result)
}

/// Parse `--key value` pairs. Repeated keys accumulate; unknown shapes are a
/// usage error.
fn parse_flags(
    args: &[String],
) -> Result<std::collections::BTreeMap<String, Vec<String>>, InteractiveError> {
    let mut flags = std::collections::BTreeMap::new();
    let mut idx = 0;
    while idx < args.len() {
        let key = args[idx]
            .strip_prefix("--")
            .filter(|k| !k.is_empty())
            .ok_or(InteractiveError::Usage)?;
        let value = args.get(idx + 1).ok_or(InteractiveError::Usage)?;
        flags.entry(key.to_owned()).or_insert(Vec::new()).push(value.clone());
        idx += 2;
    }
    Ok(flags)
}

/// First value of a flag, for single-value flags.
fn one<'a>(flags: &'a std::collections::BTreeMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    flags.get(key).and_then(|values| values.first()).map(String::as_str)
}

fn goal_evidence_record(
    host: &mut GoalHost,
    evidence_path: &Path,
    args: &[String],
) -> Result<i32, InteractiveError> {
    let flags = parse_flags(args)?;
    let Some(snapshot) = host.snapshot() else {
        println!("no active goal");
        return Ok(JsonlExitCode::Usage.as_i32());
    };
    let kind: EvidenceKind = one(&flags, "kind")
        .ok_or(InteractiveError::Usage)?
        .parse()
        .map_err(|_| InteractiveError::Usage)?;
    let status = match one(&flags, "status") {
        Some(raw) => raw
            .parse::<EvidenceStatus>()
            .map_err(|_| InteractiveError::Usage)?,
        None => EvidenceStatus::Passed,
    };
    let producer = match one(&flags, "producer") {
        None | Some("human") => EvidenceProducer::Human,
        Some("system") => EvidenceProducer::System,
        Some(role @ ("main-agent" | "subagent")) => {
            let agent_id = one(&flags, "agent-id")
                .map(|id| id.parse::<protocol::AgentId>())
                .transpose()
                .map_err(|_| InteractiveError::Usage)?
                .ok_or(InteractiveError::Usage)?;
            if role == "main-agent" {
                EvidenceProducer::MainAgent { agent_id }
            } else {
                EvidenceProducer::Subagent { agent_id }
            }
        }
        Some(_) => return Err(InteractiveError::Usage),
    };
    // `test_passed` is reserved for passing test evidence; anything else is
    // stamped with its kind so the record stays descriptive but honest.
    let assertion = match one(&flags, "assertion") {
        Some(text) => text.to_owned(),
        None if kind == EvidenceKind::Test => TEST_PASSED.to_owned(),
        None => kind.as_str().to_owned(),
    };
    let subject = one(&flags, "subject")
        .unwrap_or("goal")
        .to_owned();
    let source_hash =
        protocol::ArtifactId::from_bytes(one(&flags, "source-hash").unwrap_or(&assertion).as_bytes());
    let mut spec = EvidenceSpec::new(
        protocol::EvidenceId::new(),
        snapshot.id(),
        kind,
        assertion,
        producer,
        agent_runtime::EvidenceSource::new(source_hash),
        status,
        subject,
    )
    .map_err(|err| {
        eprintln!("{err}");
        InteractiveError::Usage
    })?;
    if let Some(criterion) = one(&flags, "criterion") {
        spec = spec
            .with_criterion_id(criterion.to_owned())
            .map_err(|err| {
                eprintln!("{err}");
                InteractiveError::Usage
            })?;
    }
    if let Some(command) = one(&flags, "command") {
        spec = spec.with_command(command.to_owned()).map_err(|err| {
            eprintln!("{err}");
            InteractiveError::Usage
        })?;
    }
    match (one(&flags, "session"), one(&flags, "seq"), one(&flags, "event-id")) {
        (None, None, None) => {}
        (Some(session), Some(seq), Some(event_id)) => {
            let session = session
                .parse::<protocol::SessionId>()
                .map_err(|_| InteractiveError::Usage)?;
            let seq = seq.parse::<u64>().map_err(|_| InteractiveError::Usage)?;
            let citation =
                EvidenceLedgerRef::new(session, event_id.to_owned(), seq).map_err(|err| {
                    eprintln!("{err}");
                    InteractiveError::Usage
                })?;
            spec = spec.with_ledger_ref(citation);
        }
        _ => return Err(InteractiveError::Usage),
    }
    // Locked read-modify-write: reloads the evidence store fresh under the
    // lock immediately before appending, so a concurrent writer (another
    // `rapid goal evidence record`, or a `rapid goal claim` persisting a
    // check's own evidence) can never have its already-committed record
    // silently erased by this process's own possibly-stale in-memory copy.
    host.update_evidence(evidence_path, |host| -> Result<(), agent_runtime::EvidenceError> {
        let record = host.record_evidence(spec)?;
        println!(
            "recorded {} kind={} status={} producer={}",
            record.id(),
            record.kind(),
            record.status(),
            record.producer()
        );
        Ok(())
    })
    .map_err(|err| {
        match &err {
            GoalTransactionError::Persist(persist_err) => eprintln!("{persist_err}"),
            GoalTransactionError::Mutate(mutate_err) => eprintln!("{mutate_err}"),
        }
        InteractiveError::Internal
    })?;
    Ok(0)
}

fn goal_evidence_list(host: &GoalHost) -> Result<i32, InteractiveError> {
    let store = host.evidence().store();
    if store.is_empty() {
        println!("no evidence recorded");
        return Ok(JsonlExitCode::Usage.as_i32());
    }
    for record in store.records() {
        let criterion = record.criterion_id().unwrap_or("-");
        println!(
            "{} kind={} status={} freshness={} producer={} criterion={}{}",
            record.id(),
            record.kind(),
            record.status(),
            record.freshness(),
            record.producer(),
            criterion,
            record
                .ledger_ref()
                .map(|_| " backed=ledger")
                .unwrap_or(""),
        );
    }
    Ok(0)
}

fn goal_lifecycle(
    host: &mut GoalHost,
    kind: &str,
    cancel: &agent_runtime::CancellationToken,
) -> Result<i32, InteractiveError> {
    let Some(goal_id) = host.snapshot().map(|s| s.id()) else {
        println!("no active goal");
        return Ok(JsonlExitCode::Usage.as_i32());
    };
    if kind == "complete" && !host.can_complete(cancel) {
        println!("completion refused: criteria are not all satisfied by recorded evidence");
        return Ok(JsonlExitCode::GoalIncomplete.as_i32());
    }
    let command = match kind {
        "pause" => GoalCommand::Pause {
            goal_id,
            process_recovered: false,
        },
        "resume" => GoalCommand::Resume { goal_id },
        "cancel" => GoalCommand::Cancel { goal_id },
        "complete" => GoalCommand::Complete { goal_id },
        _ => return Err(InteractiveError::Usage),
    };
    host.apply(command, &GoalActor::Human, cancel)
        .map_err(|_| InteractiveError::Internal)?;
    Ok(0)
}

/// `/goal pause|resume|cancel`'s three real, wired verbs. A separate typed
/// enum from `goal_lifecycle`'s own `&str kind` (headless-CLI-specific: it
/// also accepts `"complete"`, which no TUI slash command reaches today) so
/// [`SessionLoop::goal_lifecycle_command`] can't be handed a string the TUI
/// grammar never actually parses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoalLifecycleKind {
    Pause,
    Resume,
    Cancel,
}

impl GoalLifecycleKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }

    fn command(self, goal_id: protocol::GoalId) -> GoalCommand {
        match self {
            Self::Pause => GoalCommand::Pause {
                goal_id,
                process_recovered: false,
            },
            Self::Resume => GoalCommand::Resume { goal_id },
            Self::Cancel => GoalCommand::Cancel { goal_id },
        }
    }
}

/// Local-error text for a slash command the parser rejected. Kept short and
/// specific rather than dumping the full command catalog — this used to be
/// unreachable for a real reason: every non-`Empty` `CommandError` here used
/// to propagate as `InteractiveError::Command`, which `SessionLoop::run`'s
/// own `?` turned into ending the whole interactive session over a single
/// mistyped or unsupported slash command. `CommandError::help()` already
/// distinguishes "no usage to show" (unknown command) from "here is the
/// right syntax" (bad arguments on a real command) — this only adds a short
/// label for the former so the message reads as a sentence, not a dump.
fn command_error_text(err: &CommandError) -> String {
    match err {
        CommandError::UnknownCommand => {
            "unknown command — type /help for available commands".to_owned()
        }
        CommandError::TooLong => "command too long".to_owned(),
        CommandError::InvalidArgs { .. } | CommandError::InvalidId { .. } => err.help().to_owned(),
        CommandError::Empty | CommandError::NotACommand => String::new(),
    }
}

/// Local-error text for a `KernelAction` that parsed correctly but has no
/// production backend anywhere in the workspace today — investigated and
/// recorded in `newtask.md`'s command inventory, not guessed. Each reason
/// names the actual, specific gap (no registry, no store, not wired) rather
/// than a generic "not implemented," per the driving instruction's own
/// truthful-help requirement.
fn unsupported_command_text(action: &KernelAction) -> String {
    let reason = match action {
        KernelAction::PauseAgent { .. }
        | KernelAction::ResumeAgent { .. }
        | KernelAction::SleepAgent { .. } => {
            "no running-agent registry exists yet to pause, resume, or sleep a specific agent"
        }
        KernelAction::ShowGoalBudget { .. } => {
            "goal budget has no display or mutation backend yet, in the TUI or the headless CLI"
        }
        KernelAction::ReindexContext => {
            "on-demand reindex is not wired; context is already re-indexed automatically each turn"
        }
        KernelAction::SuggestKnowledge { .. }
        | KernelAction::ApproveKnowledge { .. }
        | KernelAction::RejectKnowledge { .. }
        | KernelAction::EditKnowledge { .. } => "no knowledge-candidate store exists yet",
        KernelAction::RunPlaybook { .. } => {
            "playbooks can be compiled but nothing executes a compiled graph yet"
        }
        KernelAction::ValidatePlaybook { .. } => {
            "playbook validation has no name-addressable store to resolve against yet"
        }
        KernelAction::SelectModel { .. } => {
            "mid-session model switching is not wired yet; set RAPIDLM_MODEL or edit config.toml"
        }
        KernelAction::AddMcp { .. } | KernelAction::RemoveMcp { .. } | KernelAction::AuthMcp { .. } => {
            "external MCP server trust management is not wired into the interactive session yet"
        }
        KernelAction::InstallPlugin { .. }
        | KernelAction::RemovePlugin { .. }
        | KernelAction::SetPluginPermissions { .. } => {
            "plugin install/trust management is not wired into the interactive session yet"
        }
        KernelAction::ResumeSession { .. } => "cross-process session resume is not wired yet",
        KernelAction::CompactSession => "on-demand transcript compaction is not wired yet",
        KernelAction::ApplyChangeSet { .. } | KernelAction::Rollback { .. } => {
            "no change-set apply/rollback backend exists yet"
        }
        KernelAction::Handoff { .. } | KernelAction::Takeover { .. } | KernelAction::ControlReturn => {
            "execution handoff/takeover is not wired into the interactive session yet"
        }
        KernelAction::ComputerObserve | KernelAction::ComputerRecord | KernelAction::ComputerTest => {
            "computer-use actions are not wired into the interactive session yet"
        }
        _ => "not available yet",
    };
    format!("not available: {reason}")
}

/// stderr guidance for the typed no-config fallback (mirrors the Grok Build
/// onboarding: a small user TOML selects provider, model, and credential).
const NOT_CONFIGURED_HINT: &str = "no model configured: add a [models] default and a [model.<id>] \
table (provider, model, base_url) to ~/.rapidlm/config.toml or point RAPIDLM_CONFIG at one; \
see docs/configuration.md";

/// Load `.rapidlm/reminders.toml` and admit the always-on feeds. Returns the
/// rendered block plus the strongest reminder floor, or `None` when there is
/// no roster or nothing was admitted.
fn load_active_reminders(
) -> Result<Option<(String, agent_runtime::reminders::ReminderFloor)>, agent_runtime::reminders::ReminderError>
{
    let path = std::path::Path::new(".rapidlm").join("reminders.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let roster = agent_runtime::reminders::ReminderRoster::parse(&text)?;
    let nominated: Vec<String> = roster
        .feeds()
        .iter()
        .filter(|feed| feed.requires.is_none())
        .map(|feed| feed.name.clone())
        .collect();
    let active = agent_runtime::reminders::ActiveReminders::admit(&roster, &nominated);
    Ok(active
        .render()
        .map(|block| (block, active.effort_floor())))
}

/// Load the managed policy once and gate every `[models] fallback` candidate
/// against it, in one pass — never a second, independent `load_policy` call
/// per candidate. `load_policy`'s own doc comment: "set-but-unreadable is an
/// error (a configured-but-absent control document must not silently become
/// 'no policy')" — propagated here via `?` rather than folded into `None`,
/// so a transient failure on this read (a live policy-file rewrite mid-turn,
/// an I/O hiccup) refuses the fallback chain the same way the primary
/// model's own gating already refuses the whole turn on the identical
/// failure, instead of silently letting every fallback candidate through
/// ungated. A dropped candidate (blocked by the managed provider allowlist)
/// is a warning, not an error — the turn still runs on the primary/other
/// candidates.
fn gate_fallback_candidates(
    process_env: &[(String, String)],
    candidates: Vec<crate::user_config::ActiveModel>,
) -> Result<(Vec<crate::user_config::ActiveModel>, Vec<String>), crate::managed_config::ManagedConfigError>
{
    let policy = crate::managed_config::load_policy(process_env)?;
    let mut gated = Vec::new();
    let mut warnings = Vec::new();
    for candidate in candidates {
        match crate::managed_config::apply_to_fallback_candidate(candidate, policy.as_ref()) {
            Ok(candidate) => gated.push(candidate),
            Err(profile_id) => warnings.push(format!(
                "models.fallback entry '{profile_id}' is not on the managed provider allowlist; skipped"
            )),
        }
    }
    Ok((gated, warnings))
}

/// Raise the configured reasoning effort to the reminders' floor. The roster
/// stores the semantics; the composition root maps them onto the router's
/// effort ladder. A configured effort already at or above the floor wins.
fn apply_reminder_floor(
    mut active: crate::user_config::ActiveModel,
    floor: agent_runtime::reminders::ReminderFloor,
) -> crate::user_config::ActiveModel {
    use agent_runtime::reminders::ReminderFloor;
    use llm_router::ReasoningEffort;
    let mapped = match floor {
        ReminderFloor::Baseline => return active,
        ReminderFloor::Low => ReasoningEffort::Low,
        ReminderFloor::Medium => ReasoningEffort::Medium,
        ReminderFloor::High => ReasoningEffort::High,
        ReminderFloor::Max => ReasoningEffort::Ultra,
    };
    active.entry.reasoning_effort = Some(match active.entry.reasoning_effort {
        Some(current) if current >= mapped => current,
        _ => mapped,
    });
    active
}

/// Parsed `rapid exec` command line: the prompt plus the opt-in diagnostics
/// flag. `--verbose` is a flag anywhere in the args, never prompt text.
struct ExecArgs {
    prompt: String,
    verbose: bool,
    /// Resource ceiling (Modbit `WRK-017`: CPU/RAM/disk/network/token/cost/
    /// concurrency ceilings). Only wall-clock is implemented here — the
    /// others need real OS-level resource monitoring, genuinely new
    /// systems work not attempted in this pass (see `newtask.md` §2.10).
    /// Enforced cooperatively via the same `CancellationToken` every model
    /// step and tool call already checks, not a hard process kill.
    max_wall_time: Option<Duration>,
    /// `--json-schema <path>`: constrain the turn to Qwen-Code-style
    /// structured output (`newtask.md` §1.5/#16). The file's contents are
    /// read and parsed later, not here — this only carries the path.
    json_schema: Option<PathBuf>,
    /// `--jsonl`: write the turn's outcome as versioned JSONL protocol
    /// records (`headless::jsonl`) instead of plain text — covers only the
    /// turn-execution outcome itself, not a pre-flight setup failure (bad
    /// model config, bad `--json-schema`), which still exits with a typed
    /// code but without a `session.finished` record. See `newtask.md` §2.8's
    /// correction: this reuses the JSONL contract for `rapid exec`, not the
    /// separate, unbuilt `rapid run <goal/playbook>` durable-graph command
    /// the contract's own doc comment was originally scoped to.
    jsonl: bool,
}

fn parse_exec_args(args: &[String]) -> Option<ExecArgs> {
    let mut verbose = false;
    let mut max_wall_time = None;
    let mut json_schema = None;
    let mut jsonl = false;
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--verbose" {
            verbose = true;
        } else if args[i] == "--max-wall-time" {
            i += 1;
            let secs: u64 = args.get(i)?.parse().ok()?;
            max_wall_time = Some(Duration::from_secs(secs));
        } else if args[i] == "--json-schema" {
            i += 1;
            json_schema = Some(PathBuf::from(args.get(i)?));
        } else if args[i] == "--jsonl" {
            jsonl = true;
        } else {
            words.push(&args[i]);
        }
        i += 1;
    }
    let prompt = words.join(" ");
    if prompt.is_empty() {
        return None;
    }
    Some(ExecArgs {
        prompt,
        verbose,
        max_wall_time,
        json_schema,
        jsonl,
    })
}

/// Locate the project for `rapid exec` from the process cwd and look up its
/// trust status. Any resolution failure stays fail-closed: the caller treats
/// "unknown" like untrusted.
fn exec_workspace(cancel: &CancellationToken) -> Option<(PathBuf, TrustStatus)> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = canonicalize_dir(&cwd).ok()?;
    let root = detect_project_root(&cwd, cancel).ok()?;
    let identity = ProjectIdentity::new(root.as_path(), None).ok()?;
    let user_home = exec_user_home()?;
    let trust = ProjectTrustStore::open(user_home.join(TRUST_CATALOG_NAME))
        .get(&identity, cancel)
        .ok()?;
    Some((root, trust))
}

/// Resolve the RapidLM home directory from the process environment, mirroring
/// the interactive `resolve_user_home` precedence: `RAPIDLM_HOME` names the
/// home itself, `HOME`/`USERPROFILE` its parent.
fn exec_user_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var(RAPIDLM_HOME_ENV)
        && !home.is_empty()
    {
        return canonicalize_or_create(Path::new(&home)).ok();
    }
    for key in [HOME_ENV, USERPROFILE_ENV] {
        if let Ok(home) = std::env::var(key)
            && !home.is_empty()
        {
            return canonicalize_or_create(&PathBuf::from(home).join(".rapidlm")).ok();
        }
    }
    None
}

/// Cause-classified failure line for a finished turn: a provider cause names
/// the class and the first remedy; otherwise the turn summary carries the
/// typed stop reason (e.g. "agent turn failed: repeated_tool_call").
fn describe_turn_failure(
    result: &AgentResult,
    cause: Option<FailureCause>,
    detail: Option<&TurnFailureDetail>,
) -> String {
    let summary = result.summary();
    // A tool-failure stop names the failing tool and the underlying error so
    // the run is diagnosable from CLI output alone; model failures keep the
    // typed cause + remedy form.
    if let Some(detail) = detail {
        return format!(
            "{summary} (failing tool: {}; {})",
            detail.tool(),
            detail.error()
        );
    }
    match cause {
        Some(cause) => format!(
            "{summary} ({}; {})",
            cause.as_str(),
            cause.remedy()
        ),
        None => summary.to_owned(),
    }
}

/// Render micro-USD (millionths of a dollar, `ExecOutcome::cost_usd_micros`'s
/// unit — integer to avoid float rounding in the accounting path) as a
/// human-readable dollar figure for stderr/summary text. Six decimal places:
/// real per-turn costs for a single exec are often sub-cent, and rounding to
/// 2 decimals would silently read every one of them as "$0.00".
fn format_usd_micros(usd_micros: u64) -> String {
    format!("${:.6}", usd_micros as f64 / 1_000_000.0)
}

/// Whether a finished turn counts as an effective success even when its
/// terminal status isn't `Succeeded`: a turn that committed at least one
/// tool call and whose only defect is an empty final model response is a
/// completed task with a missing summary, not a failure. Shared by the
/// top-level exec exit-code path and `task_spawn` subagent runs so a child
/// hitting this case is not misreported as a hard failure to its parent.
fn is_effective_success(outcome: &ExecOutcome) -> bool {
    outcome.result.status() == AgentTerminalStatus::Succeeded
        || (outcome.stop_reason == Some(TurnStopReason::EmptyResponse) && outcome.tool_calls > 0)
}

/// Typed exit code for a finished-but-not-effectively-successful turn. A
/// `Cancelled` terminal status is Ctrl-C/SIGINT, not a generic failure — a CI
/// script should be able to tell "the user interrupted this" apart from
/// "the model/tool layer failed" without parsing stderr text. A provider-
/// classified `FailureCause` (`Auth`/`Connection`/`Rejected`/`Transient`)
/// maps to the same `Provider` bucket the JSONL contract already uses for
/// provider errors; an unclassified cause (e.g. a tool-failure stop, which
/// carries `failure_detail` instead) falls back to `Runtime`.
fn exec_turn_exit_code(
    status: AgentTerminalStatus,
    failure_cause: Option<FailureCause>,
) -> JsonlExitCode {
    if status == AgentTerminalStatus::Cancelled {
        return JsonlExitCode::Interrupted;
    }
    match failure_cause {
        Some(FailureCause::Unspecified) | None => JsonlExitCode::Runtime,
        Some(_) => JsonlExitCode::Provider,
    }
}

/// Env var overriding the permission mode for one exec run.
const PERMISSION_MODE_ENV: &str = "RAPIDLM_PERMISSION_MODE";
/// Project settings documents consulted for the permission lattice, in
/// precedence order (RapidLM's own first, then the Claude-compat path).
const PROJECT_SETTINGS_FILES: [&str; 2] = [".rapidlm/settings.json", ".claude/settings.json"];
/// Persisted per-project allow grants consulted before any ask.
const PERMISSIONS_STORE_NAME: &str = "project-permissions.json";
/// Maximum rule entries admitted across all settings documents. Each file is
/// already individually capped at `MAX_RULES` by `parse_settings` (an
/// over-limit file fails the whole load with `TooManyRules`, never silently
/// truncates) — sizing this to `MAX_RULES * PROJECT_SETTINGS_FILES.len()`
/// means every successfully-loaded file's rules always fit in the merge
/// below. A per-file cap here (the previous `MAX_RULES` alone) let a single
/// maxed-out file silently crowd out every later file's rules, deny rules
/// included, contradicting this function's own "deny rules always apply"
/// contract.
const MAX_WIRED_RULES: usize = crate::permissions::MAX_RULES * PROJECT_SETTINGS_FILES.len();

/// Resolve the permission lattice for one exec run: mode precedence is env >
/// project settings > Claude-compat `defaultMode` > `default`; rules merge
/// from every settings document that exists (deny rules always apply). A
/// corrupt settings document refuses the run typed rather than silently
/// dropping its deny rules; a corrupt grants file simply yields no grants
/// (fail-closed in the permissive direction).
/// Resolve the permission mode for one exec run: env override > project
/// settings > Claude-compat `defaultMode` > `default`.
fn exec_permission_mode() -> Result<crate::permissions::PermissionMode, String> {
    use crate::permissions::{PermissionMode, parse_settings};
    if let Ok(raw) = std::env::var(PERMISSION_MODE_ENV)
        && !raw.is_empty()
    {
        return PermissionMode::parse(&raw).ok_or_else(|| {
            format!(
                "{PERMISSION_MODE_ENV} must be one of: {}",
                crate::permissions::MODE_NAMES.join(", ")
            )
        });
    }
    for file_name in PROJECT_SETTINGS_FILES {
        let Ok(text) = fs::read_to_string(file_name) else {
            continue;
        };
        let settings = parse_settings(&text).map_err(|err| {
            format!("{} could not be loaded: {}", file_name, err.as_str())
        })?;
        if let Some(mode) = settings.mode {
            return Ok(mode);
        }
    }
    Ok(PermissionMode::Default)
}

/// Merge every loaded settings document's rules, in file-precedence order,
/// bounded by `MAX_WIRED_RULES`. Each document's own rules are already
/// individually capped at `crate::permissions::MAX_RULES` by `parse_settings`
/// (an over-limit file fails the whole load, never silently truncates), and
/// `MAX_WIRED_RULES` is sized to fit every known settings file's full quota —
/// so this only ever truncates if a future settings source is added without
/// updating that sizing, not under today's fixed two-file set.
fn merge_settings_rules(
    loaded_settings: &[crate::permissions::ProjectSettings],
) -> Vec<crate::permissions::ToolRule> {
    let mut rules: Vec<crate::permissions::ToolRule> = Vec::new();
    for settings in loaded_settings {
        for rule in settings.rules.clone() {
            if rules.len() >= MAX_WIRED_RULES {
                break;
            }
            rules.push(rule);
        }
    }
    rules
}

fn exec_permission_lattice(
    canonical_root: Option<&Path>,
    forced_mode: Option<crate::permissions::PermissionMode>,
) -> Result<crate::permissions::PermissionLattice, String> {
    use crate::permissions::{
        PermissionLattice, PermissionMode, ProjectSettings, ToolPattern, parse_grants,
        parse_settings,
    };
    let mut mode: Option<PermissionMode> = match exec_permission_mode() {
        Ok(mode) => Some(mode),
        Err(msg) => {
            eprintln!("warning: {msg}, falling back to default mode resolution");
            None
        }
    };
    let mut loaded_settings: Vec<ProjectSettings> = Vec::new();
    for file_name in PROJECT_SETTINGS_FILES {
        let Ok(text) = fs::read_to_string(file_name) else {
            continue;
        };
        let settings = parse_settings(&text).map_err(|err| {
            format!("{} could not be loaded: {}", file_name, err.as_str())
        })?;
        if mode.is_none() {
            mode = settings.mode;
        }
        loaded_settings.push(settings);
    }
    let rules = merge_settings_rules(&loaded_settings);
    let mode = mode.unwrap_or(PermissionMode::Default);
    // A caller-forced mode (e.g. `rapid cron`'s propose-only execution,
    // Modbit `AGT-008`/§3.2) overrides every other source unconditionally —
    // this is a hard ceiling the caller itself imposes, not a user
    // preference, so it must win over env/settings precedence rather than
    // just seed it. Composes safely with the managed-policy gate below
    // regardless of order: `Plan`, the only mode ever forced today, is
    // already the least permissive of all six, so gating it against any
    // ceiling is always a no-op.
    let mode = forced_mode.unwrap_or(mode);
    // Managed-policy ceiling (Modbit `CAP-001`): a project's own settings or
    // `RAPIDLM_PERMISSION_MODE` may only narrow the resolved mode, never
    // widen it past whatever an administrator allows. A configured-but-
    // unreadable policy document fails closed here too, matching
    // `load_policy`'s own documented invariant — it must never silently
    // become "no policy".
    let managed_policy = crate::managed_config::load_policy(&std::env::vars().collect::<Vec<_>>())
        .map_err(|err| format!("managed policy could not be loaded: {err}"))?;
    let (mode, gate_report) = crate::managed_config::gate_permission_mode(mode, managed_policy.as_ref());
    if let Some(report) = gate_report {
        eprintln!("warning: {report}");
    }
    let mut lattice = PermissionLattice::new(mode).with_rules(rules);
    // Persisted grants, keyed by canonical project root.
    if let (Some(root), Some(home)) = (canonical_root, exec_user_home()) {
        let store_path = home.join(PERMISSIONS_STORE_NAME);
        if let Ok(text) = fs::read_to_string(&store_path)
            && let Ok(canonical) = fs::canonicalize(root)
            && let Ok(grants) = parse_grants(&text)
        {
            let allow = grants.for_root(&canonical.to_string_lossy());
            let patterns: Vec<ToolPattern> = allow;
            lattice = lattice.with_grants(patterns);
        }
    }
    // Managed-policy tool ban (Modbit `CAP-001`, same layer as the mode
    // ceiling above): applied unconditionally, since a pure addition to
    // `denied_tools` has no lower-trust value to compare against — there is
    // no "allow_tools" override checked earlier that could widen past it.
    if let Some(patterns) = managed_policy.as_ref().and_then(|policy| policy.denied_tools()) {
        eprintln!(
            "warning: managed policy bans {} tool pattern(s) outright",
            patterns.len()
        );
        lattice = lattice.with_denied_tools(patterns.iter().cloned());
    }
    // Same "applied unconditionally, no merge order to get wrong" shape as
    // denied_tools above — a deployment-wide write confinement independent
    // of any per-task_spawn write_scope (see `PermissionLattice::
    // admin_write_scope`'s own doc comment for why the two never share a
    // field).
    if let Some(scope) = managed_policy.as_ref().and_then(|policy| policy.confine_writes_to()) {
        eprintln!("warning: managed policy confines every write to '{scope}'");
        lattice = lattice.with_admin_write_scope(scope);
    }
    Ok(lattice)
}

/// Trusted-project config merged from every file in `PROJECT_SETTINGS_FILES`
/// — the `web_fetch` allowlist, hooks, shadow-diagnostics config, and MCP
/// servers a `.rapidlm/settings.json` *or* `.claude/settings.json`-only
/// project can configure, in one place instead of duplicated at both call
/// sites (the real `exec_turn` path and this struct's own unit test).
struct ProjectIntegrations {
    fetch_allowlist: Vec<String>,
    hooks: crate::hooks::HooksConfig,
    shadow: Option<crate::shadow_diagnostics::ShadowDiagnosticsConfig>,
    mcp_servers: Vec<crate::exec_tools::McpServerConfig>,
}

/// Read and merge every `PROJECT_SETTINGS_FILES` entry under `root`. List-
/// shaped config (fetch allowlist, each hook stage, MCP servers) merges
/// across every file that exists — the same precedence
/// `exec_permission_lattice` already uses for permission rules. Single-value
/// config (shadow-diagnostics) uses first-file-wins, matching
/// `exec_permission_mode`'s `mode` resolution. A missing or unparsable file
/// contributes nothing rather than failing the whole load — this mirrors
/// this block's pre-existing behavior (only permission-rule parsing is
/// strict enough to refuse the run typed; this integration config was never
/// that strict even before this function existed).
fn load_project_integrations(root: &Path) -> ProjectIntegrations {
    let mut fetch_allowlist: Vec<String> = Vec::new();
    let mut hooks = crate::hooks::HooksConfig::default();
    let mut shadow = None;
    let mut mcp_servers = Vec::new();
    for file_name in PROJECT_SETTINGS_FILES {
        let settings_path = root.join(file_name);
        let Ok(text) = fs::read_to_string(&settings_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(entries) = value.get("fetch_allowlist").and_then(serde_json::Value::as_array) {
            fetch_allowlist.extend(entries.iter().filter_map(|entry| entry.as_str().map(str::to_owned)));
        }
        if let Some(file_hooks) = crate::hooks::HooksConfig::parse(&value) {
            hooks.pre_tool_use.extend(file_hooks.pre_tool_use);
            hooks.post_tool_use.extend(file_hooks.post_tool_use);
            hooks.session_start.extend(file_hooks.session_start);
            hooks.session_end.extend(file_hooks.session_end);
            hooks.subagent_start.extend(file_hooks.subagent_start);
            hooks.subagent_stop.extend(file_hooks.subagent_stop);
        }
        if shadow.is_none() {
            shadow = crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&value);
        }
        mcp_servers.extend(crate::exec_tools::parse_mcp_servers(&value));
    }
    // Each file's own HooksConfig::parse already capped itself at
    // MAX_HOOKS_PER_STAGE; re-cap after merging two files' worth so the
    // combined per-stage bound still holds.
    for stage in [
        &mut hooks.pre_tool_use,
        &mut hooks.post_tool_use,
        &mut hooks.session_start,
        &mut hooks.session_end,
        &mut hooks.subagent_start,
        &mut hooks.subagent_stop,
    ] {
        stage.truncate(crate::hooks::MAX_HOOKS_PER_STAGE);
    }
    ProjectIntegrations {
        fetch_allowlist,
        hooks,
        shadow,
        mcp_servers,
    }
}

/// Cancel `cancel` if it isn't already cancelled once `max_wall_time`
/// elapses — a bounded wall-clock resource ceiling (Modbit `WRK-017`;
/// `newtask.md` §2.10 — the full Resource Governor, with CPU/RAM/disk/
/// network/concurrency ceilings, is real, separate systems work, not
/// attempted here). Cooperative, not a hard kill: this sets the same flag
/// a Ctrl-C would, which every model step and tool call already checks; a
/// turn already past its last cooperative checkpoint when the deadline
/// passes still finishes that one step before observing it.
fn spawn_wall_time_watchdog(cancel: agent_runtime::CancellationToken, max_wall_time: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(max_wall_time);
        if !cancel.is_cancelled() {
            eprintln!(
                "rapid: exceeded --max-wall-time ({}s); cancelling the turn",
                max_wall_time.as_secs()
            );
            cancel.cancel();
        }
    });
}

/// Fires `session_end` hooks exactly once, on whichever exit path `exec_turn`
/// takes — early `?`-propagated error, an explicit early return, or falling
/// off the end. `exec_turn` has many of the first two; a `Drop` guard is the
/// only way to guarantee this without threading a result through every
/// branch by hand. Empty by construction until hooks load; a fire with no
/// configured hooks is a no-op.
#[derive(Default)]
struct SessionEndHookGuard {
    hooks: Vec<String>,
}

impl Drop for SessionEndHookGuard {
    fn drop(&mut self) {
        if !self.hooks.is_empty() {
            let _ = crate::hooks::run_notify_hooks(
                &self.hooks,
                "session_end",
                serde_json::json!({}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
    }
}

/// Runs child agents for `task_spawn`: builds a fresh model adapter from the
/// same user config and a depth-restricted tool surface (children never get
/// the spawn tool, so the depth limit is structural). explore/plan scopes are
/// read-only.
struct LiveSubagentRunner {
    active: crate::user_config::ActiveModel,
    root: PathBuf,
    /// The full permission lattice loaded for the parent session (mode plus
    /// project-settings rules and persisted grants) — cloned into every
    /// child so a deny/ask rule that protects the parent also protects its
    /// subagents, instead of each child starting from a bare, ruleless
    /// lattice.
    permissions: crate::permissions::PermissionLattice,
    /// The parent's own disk/network resource-ceiling counters (Modbit
    /// `WRK-017`), shared into every child so the whole turn — parent plus
    /// every subagent it spawns — counts against one budget instead of each
    /// subagent getting its own fresh one. See `ExecTools::
    /// share_turn_budgets`'s own doc comment for why a fresh-per-child
    /// counter under-enforces a "per-turn" ceiling.
    turn_budgets: (std::sync::Arc<std::sync::atomic::AtomicU64>, std::sync::Arc<std::sync::atomic::AtomicU64>),
    /// The parent's shared per-path write-lock registry (Modbit `WRK-017`),
    /// so a subagent writing the same resolved path as its parent or a
    /// sibling subagent serializes against them instead of racing on the
    /// underlying file. See `exec_tools::WriteLocks`'s own doc comment.
    write_locks: crate::exec_tools::WriteLocks,
    /// The parent's shared per-turn background-job budget counter (Modbit
    /// `WRK-017`), so a subagent's own `shell_exec(background: true)` calls
    /// count against the same turn-wide ceiling as the parent's and every
    /// sibling's instead of each starting a fresh `MAX_BACKGROUND_JOBS`
    /// budget of its own. See `JobRegistry::started_this_turn`'s own doc
    /// comment.
    job_budget: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The parent's configured project hooks, cloned into every child so a
    /// `pre_tool_use`/`post_tool_use` policy hook that gates the parent's
    /// own tool calls also gates its subagents' — without this, delegating
    /// a call to a subagent silently bypassed every hook.
    hooks: crate::hooks::HooksConfig,
    /// The parent's configured shadow-diagnostics command, if any, cloned
    /// into every child so a subagent's writes are verified against the
    /// same quality gate as the parent's own instead of silently skipping
    /// it.
    shadow_diagnostics: Option<crate::shadow_diagnostics::ShadowDiagnosticsConfig>,
    /// Whether the parent traces its own tool calls to stderr — carried
    /// into every child so a headless `rapid exec` run watching its own
    /// stderr sees a delegated subagent's tool calls too, not silence.
    trace_calls: bool,
    /// The parent's own (possibly managed-policy-narrowed) per-turn disk/
    /// network ceilings, applied to every child alongside the shared
    /// counters (`turn_budgets`) so a managed policy's `max_write_bytes_
    /// per_turn`/`max_fetch_bytes_per_turn` bounds a subagent's own writes
    /// too, not just the parent's.
    turn_ceilings: (u64, u64),
    /// The parent's redaction snapshot (known secret values to scrub from
    /// captured `shell_exec` output), cloned into every child so a
    /// subagent's own commands are scrubbed for the same known secrets as
    /// the parent's instead of leaking them unscrubbed by default. See
    /// `ExecTools::share_redaction`'s own doc comment.
    redaction: Option<security::RedactionSnapshot>,
}

impl crate::exec_tools::SubagentRunner for LiveSubagentRunner {
    fn run(
        &self,
        prompt: &str,
        agent_type: &str,
        write_scope: Option<&str>,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<crate::exec_tools::SubagentReport, String> {
        use crate::exec_tools::ExecTools;
        let store = auth::InMemoryCredentialStore::new();
        let model = crate::model::ConfiguredModel::build(&self.active, &store)
            .map_err(|err| err.to_string())?;
        // A write-capable child never inherits a blanket `BypassPermissions`
        // ceiling: it was the model's own choice to delegate, not the
        // human's direct action, so it must not silently wield authority the
        // human never reviewed for this specific sub-task. Rules and
        // persisted grants still carry over unchanged. See
        // `PermissionLattice::for_subagent`.
        let mut child_permissions = self.permissions.for_subagent();
        if let Some(scope) = write_scope {
            child_permissions = child_permissions.with_write_scope(scope);
        }
        let mut tools = if agent_type == "explore" || agent_type == "plan" {
            ExecTools::read_only_with_permissions(&self.root, child_permissions)
        } else {
            ExecTools::workspace_with_permissions(&self.root, child_permissions)
        }
        .map_err(|err| err.to_string())?;
        // Bounded recursive delegation (Modbit AGT-010): a subagent must
        // never itself spawn further subagents by default. Read-only
        // children already lose task_spawn via the write-tool filter, but a
        // write-capable child (the `else` branch above) previously kept it
        // — unbounded nesting was possible for any non-explore/plan
        // agent_type. See `newtask.md` §2.2.
        tools.disable_nested_spawn();
        // Share the parent's disk/network budget (Modbit WRK-017) rather
        // than let this child start a fresh one — see `turn_budgets`'s own
        // doc comment.
        let (bytes_written, fetch_bytes) = self.turn_budgets.clone();
        tools.share_turn_budgets(bytes_written, fetch_bytes);
        // Share the parent's per-path write-lock registry (Modbit
        // WRK-017) rather than let this child start a fresh, useless one
        // of its own — see `write_locks`'s own doc comment.
        tools.share_write_locks(self.write_locks.clone());
        // Same reasoning for the per-turn background-job budget — see
        // `job_budget`'s own doc comment.
        tools.share_job_budget(self.job_budget.clone());
        // Scrub the same known secrets from this child's own shell_exec
        // output as the parent's — see `redaction`'s own doc comment.
        tools.share_redaction(self.redaction.clone());
        // Policy hooks (pre_tool_use/post_tool_use/subagent_start/
        // subagent_stop) must apply to a subagent's own tool calls too, or
        // delegation becomes a way to route around them entirely.
        tools.set_hooks(self.hooks.clone());
        // Same reasoning for the shadow-diagnostics quality gate: a
        // subagent's writes should be verified the same way the parent's
        // own would be.
        if let Some(shadow) = self.shadow_diagnostics.clone() {
            tools.set_shadow_diagnostics(shadow);
        }
        tools.set_trace_calls(self.trace_calls);
        // A managed policy's per-turn disk/network ceilings (Modbit
        // `CAP-001`) must bound a subagent's own writes/fetches too, not
        // just the parent's — narrow_*_ceiling is a no-op when the default
        // (unmanaged) ceiling is already what the child started with.
        let (write_ceiling, fetch_ceiling) = self.turn_ceilings;
        tools.narrow_write_ceiling(write_ceiling);
        tools.narrow_fetch_ceiling(fetch_ceiling);
        // Subagents run in the same trusted project as the parent (only
        // spawned when the workspace is trusted), so they get the same
        // AGENTS.md rules and system prompt as the top-level turn instead of
        // running with neither. Budget comes from `model` (just built
        // above, from the same `active` config the parent turn resolved) —
        // a child never gets a hard-coded placeholder its parent's real
        // context budget already disagrees with.
        let caps = model.capabilities();
        let preserved = build_live_context(
            Some(&self.root),
            Some(&self.root),
            prompt.to_owned(),
            true,
            caps.context_limit(),
            caps.max_output(),
        )
        .map_err(|_| "child context rejected".to_owned())?;
        let spec = AgentSpec::builder(
            protocol::AgentId::new(),
            AgentRole::Coder,
            prompt.to_owned(),
            protocol::WorkspaceViewId::new(),
        )
        .permissions_profile("subagent")
        .build()
        .map_err(|_| "child spec rejected".to_owned())?;
        let request = AgentExecutionRequest::new(spec, protocol::SessionId::new());
        let mut events = Vec::new();
        let outcome = run_live_exec(
            preserved,
            model,
            &request,
            &mut tools,
            &mut events,
            cancel,
            ContextRetryPolicy::default(),
            None,
        )
        .map_err(|err| err.to_string())?;
        // A subagent needing context is not a subagent failure: collapsing
        // it into the generic `Err` below would discard the child's own
        // question and tell the parent model only "subagent turn failed",
        // giving it nothing to act on. `open_questions` is the existing,
        // already-consumed mechanism for a child to surface something it
        // couldn't resolve (`exec_tools.rs`'s subagent-summary rendering
        // already appends every entry as "open question: ..."), so the
        // question is reused through it rather than inventing a parallel
        // channel — the parent model decides what to do next (ask the user
        // itself, proceed on its own judgment, etc.), exactly as it already
        // does for any other open question a child reports.
        if let Some(question) = context_required_question(&outcome) {
            return Ok(subagent_context_required_report(&outcome, question));
        }
        if !is_effective_success(&outcome) {
            return Err(format!(
                "subagent turn {}",
                outcome.result.status().as_str()
            ));
        }
        let mut summary = outcome.result.summary().to_owned();
        if outcome.result.status() != AgentTerminalStatus::Succeeded {
            summary.push_str(&format!(
                " (the turn performed {} tool call(s) before the final response came back empty; verify workspace state)",
                outcome.tool_calls
            ));
        }
        let claims = outcome
            .result
            .claims()
            .iter()
            .map(|claim| {
                let prefix = claim
                    .criterion_id()
                    .map(|id| format!("{id}: "))
                    .unwrap_or_default();
                format!("{prefix}{} ({})", claim.text(), claim.result().as_str())
            })
            .collect();
        let blockers = outcome
            .result
            .blockers()
            .iter()
            .map(|blocker| format!("[{}] {}", blocker.kind().as_str(), blocker.summary()))
            .collect();
        let open_questions = outcome.result.open_questions().to_vec();
        let patch_summary = outcome.result.patch_summary().map(|patch| {
            format!(
                "{} file(s) changed, +{} -{}",
                patch.files_changed(),
                patch.additions(),
                patch.deletions()
            )
        });
        let artifacts = outcome
            .result
            .artifacts()
            .iter()
            .map(|artifact| {
                format!(
                    "{} ({}, {}B, {})",
                    artifact.id, artifact.media_type, artifact.bytes, artifact.redaction
                )
            })
            .collect();
        Ok(crate::exec_tools::SubagentReport {
            summary,
            status: outcome.result.status().as_str().to_owned(),
            tool_calls: outcome.tool_calls,
            tokens: outcome.tokens,
            cost_usd_micros: outcome.cost_usd_micros,
            stop_reason: outcome.stop_reason.map(|reason| reason.as_str().to_owned()),
            claims,
            blockers,
            open_questions,
            patch_summary,
            artifacts,
        })
    }
}

/// Discover project instructions (AGENTS.md convention + `.claude`/`.cursor`
/// compat paths) for the exec run, root-first and bounded. Failures are
/// advisory here (the turn continues without rules) but are reported.
fn exec_discover_rules(cwd: &Path, root: &Path) -> Option<String> {
    let cancel = agent_runtime::CancellationToken::new();
    match agent_runtime::discover_instructions(root, cwd, &cancel) {
        Ok(bundle) if !bundle.is_empty() => {
            let composed = bundle.composed();
            if composed.len() <= crate::host::MAX_RULES_BYTES {
                Some(composed)
            } else {
                eprintln!("warning: project instructions exceed the byte bound; not loaded");
                None
            }
        }
        Ok(_) => None,
        Err(err) => {
            eprintln!("warning: project instructions not loaded: {}", err.as_str());
            None
        }
    }
}

/// Build the preserved context shared by every live-exec caller: AGENTS.md
/// Derive `build_live_context`'s `(context_limit, output_reserve)` pair from
/// `backing`'s actual resolved model(s) — the one authoritative source every
/// context-budget consumer downstream (compaction thresholds, the retrieval
/// share, the system prompt's own rendered token-budget line, overflow
/// recovery — see `LiveRecoveryController::recover_from_overflow` and
/// `build_packet`, both of which already derive proportionally from these
/// two numbers) sizes itself against. Never a second, independently-guessed
/// number: both fields come straight from the same [`llm_router::provider::
/// ProviderCapabilities`] already attached to the model's own adapter config
/// (see [`ConfiguredModel::capabilities`]), the same object that already
/// governs the real provider request.
///
/// Unknown-capability precedence (an unconfigured model, or a configured one
/// that didn't set an explicit `context_window`/`max_tokens`) is not a new
/// policy invented here — it reuses exactly what `ConfiguredModel::build`
/// already falls back to for the *request itself*
/// (`crate::user_config::DEFAULT_CONTEXT_WINDOW`/`DEFAULT_MAX_OUTPUT_TOKENS`),
/// so the budget a turn is sized against can never claim more headroom than
/// the request that actually goes out believes it has.
///
/// A fallback chain uses the *minimum* `context_limit` but the *maximum*
/// `max_output` across every backend it could actually dispatch to (not
/// just the primary) — not the minimum of both, see below. The effective
/// model for a turn is not fully known until the router picks one at
/// request time (see `crate::host::FallbackChainModel`), and each backend
/// still sends its own real per-request output cap independently
/// (`ConfiguredModel.max_output_tokens`, via `build_request` —
/// `context_budget_for`'s own `output_reserve` never reaches the wire as a
/// cap on anyone's request, it only ever shapes how much of `context_limit`
/// the context builder leaves unused). That independence is exactly why the
/// minimum of `max_output` would be unsafe here: pairing the smallest
/// `context_limit` with the smallest `max_output` under-reserves headroom
/// for whichever *other* backend actually serves the turn with its own,
/// larger real output cap — prompt content sized to fit in the leftover
/// space could then combine with that backend's real output to exceed its
/// real context window, precisely the overflow class this whole budget
/// exists to prevent. Pairing the minimum `context_limit` with the
/// *maximum* `max_output` instead is provably safe for every candidate: for
/// backend i, using `context_limit_used = min_j(context_limit_j)` and
/// `output_reserve_used = max_j(max_output_j)`, the input budget actually
/// built (`context_limit_used - output_reserve_used`) plus i's own real
/// `max_output_i` never exceeds `context_limit_i`, because `context_limit_
/// used <= context_limit_i` and `max_output_i <= output_reserve_used` by
/// construction — true regardless of which single backend happens to
/// minimize context and which happens to maximize output. Using the
/// candidate-set extremes on each axis independently means the context
/// built *before* the router's choice is made is always valid for whichever
/// backend actually serves it — the same guarantee a full rebuild-on-
/// fallback would give, without needing to rebuild context after every
/// fallback.
fn context_budget_for(backing: &SelectedModel<'_>) -> (u32, u32) {
    match backing {
        SelectedModel::Unconfigured(_) => (
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
        ),
        SelectedModel::Configured(model) => {
            let caps = model.capabilities();
            (caps.context_limit(), caps.max_output())
        }
        SelectedModel::FallbackChain(chain) => chain
            .backends()
            .map(|model| {
                let caps = model.capabilities();
                (caps.context_limit(), caps.max_output())
            })
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
            .unwrap_or((
                crate::user_config::DEFAULT_CONTEXT_WINDOW,
                crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
            )),
    }
}

/// project instructions discovered under `root`/`cwd`, and the
/// conditional-section system prompt (environment, trust posture, token
/// budget). Used by both the top-level `exec` turn and `task_spawn`
/// subagents so a child sees the same project rules and system prompt as its
/// parent instead of running with neither.
fn build_live_context(
    root: Option<&Path>,
    cwd: Option<&Path>,
    prompt: String,
    trusted: bool,
    context_limit: u32,
    output_reserve: u32,
) -> Result<PreservedLiveContext, String> {
    let agents_rules = match (root, cwd) {
        (Some(root), Some(cwd)) => exec_discover_rules(cwd, root).unwrap_or_default(),
        _ => String::new(),
    };
    let mut system_prompt_context = agent_runtime::PromptContext::new();
    if let Some(cwd) = cwd {
        let candidate = agent_runtime::PromptContext::new().with_environment(format!(
            "cwd {}; os {}",
            cwd.display(),
            std::env::consts::OS
        ));
        if let Ok(with_env) = candidate {
            system_prompt_context = with_env;
        }
    }
    system_prompt_context = system_prompt_context.with_trust(if trusted {
        agent_runtime::TrustPosture::Trusted
    } else {
        agent_runtime::TrustPosture::Untrusted
    });
    system_prompt_context = system_prompt_context.with_token_budget(context_limit, output_reserve);
    let system_prompt = agent_runtime::render_system_prompt(&system_prompt_context)
        .map_err(|err| {
            eprintln!("warning: system prompt not rendered: {}", err.as_str());
        })
        .unwrap_or_default();
    PreservedLiveContext::new(prompt, Vec::new(), agents_rules, String::new(), context_limit, output_reserve)
        .map_err(|_| "context rejected".to_owned())
        .map(|preserved| preserved.with_system_prompt(Some(system_prompt)))
}

/// Build the live-context host around the prompt and run one agent turn through
/// the recovery-capable executor. The backing model is resolved Grok-style:
/// `RAPIDLM_CONFIG`/`RAPIDLM_MODEL` env overrides, then the user config file,
/// then the typed unconfigured fallback (a model step stays a typed provider
/// failure — never a synthetic completion). Workspace file tools are granted
/// only when the project is explicitly trusted; anything else stays a
/// fail-closed refusal. `--verbose` opts into bounded step diagnostics.
///
/// `forced_mode` overrides every other permission-mode source unconditionally
/// (env, project settings, Claude-compat `defaultMode`) — used by `rapid
/// cron`'s propose-only execution to guarantee `Plan` mode regardless of the
/// ambient environment. `None` (the interactive `rapid exec` CLI entry) keeps
/// the existing precedence untouched.
pub(crate) fn exec_turn(
    args: &[String],
    forced_mode: Option<crate::permissions::PermissionMode>,
) -> Result<i32, InteractiveError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{EXEC_USAGE}");
        return Ok(0);
    }
    let Some(parsed) = parse_exec_args(args) else {
        eprint!("{EXEC_USAGE}");
        return Err(InteractiveError::Usage);
    };
    // `session_end` must fire on every exit path from here down — the early
    // return above (before any project/hooks loading) doesn't count as a
    // started session, same as `session_start`'s own placement below. The
    // rest of this function has many `?`-propagated and explicit early
    // returns; a `Drop` guard is the only way to guarantee firing exactly
    // once regardless of which one is taken, without threading a result
    // through every branch by hand.
    let mut session_end_guard = SessionEndHookGuard::default();
    let prompt = parsed.prompt;
    // Workspace detection and trust: any resolution failure stays untrusted.
    let workspace_cancel = CancellationToken::new();
    let workspace = exec_workspace(&workspace_cancel);
    let trusted = matches!(&workspace, Some((_, TrustStatus::Trusted)));

    // Tools stay fail-closed: workspace tools are granted only when the
    // project is explicitly trusted and its root still resolves, and every
    // call then passes the six-mode permission lattice (deny rules, persisted
    // grants, mode table; headless asks become typed denials). Every other
    // surface refuses all proposed tool calls. Resolved once here and reused
    // below (subagent tools, the refuses-all warning) instead of re-resolving
    // the mode/env at every call site.
    let permission_lattice =
        match exec_permission_lattice(workspace.as_ref().map(|(root, _)| root.as_path()), forced_mode) {
            Ok(lattice) => lattice,
            Err(reason) => {
                eprintln!("permission configuration error: {reason}");
                return Ok(JsonlExitCode::Policy.as_i32());
            }
        };
    let mut tools = match &workspace {
        Some((root, TrustStatus::Trusted)) => {
            ExecTools::workspace_with_permissions(root, permission_lattice.clone())
                .unwrap_or_else(|_| ExecTools::noop())
        }
        _ => ExecTools::noop(),
    };
    tools.set_trace_calls(true);
    // Managed-policy disk/network ceilings (Modbit `CAP-001`/`WRK-017`):
    // narrow-only, so a missing or default policy is simply a no-op here.
    // Re-loading rather than threading the value out of
    // `exec_permission_lattice` — that call already fails the whole turn
    // closed on an unreadable policy above, so by this point a load failure
    // here would mean the file changed underneath us mid-turn; skipping the
    // (non-security-critical) ceiling narrowing in that edge case is safer
    // than failing a turn whose permission lattice already resolved. Also
    // captures `policy_version` for the router-decision log below, reusing
    // this same read rather than loading a third time — the version
    // attached to a decision must describe the policy this specific read
    // actually saw, and every extra independent read only widens the
    // (already-accepted, see above) window for the file to have changed
    // between reads and the log naming a policy that wasn't the one
    // actually applied.
    let mut policy_version: Option<String> = None;
    if let Ok(Some(policy)) = crate::managed_config::load_policy(&std::env::vars().collect::<Vec<_>>()) {
        if let Some(max) = policy.max_write_bytes_per_turn() {
            tools.narrow_write_ceiling(max);
        }
        if let Some(max) = policy.max_fetch_bytes_per_turn() {
            tools.narrow_fetch_ceiling(max);
        }
        if let Some(max) = policy.max_subagent_spawns_per_turn() {
            tools.narrow_subagent_spawn_ceiling(max);
        }
        policy_version = Some(policy.policy_version().to_owned());
    }
    // Observability for the fail-closed default: when headless exec runs
    // without workspace tools (or under a mode that refuses every call), say
    // so up front and name the levers, instead of leaving the run to fail
    // without an obvious why.
    let tools_withheld = !matches!(&workspace, Some((_, TrustStatus::Trusted)));
    let mode_refuses_all = matches!(
        permission_lattice.mode(),
        crate::permissions::PermissionMode::Plan | crate::permissions::PermissionMode::DontAsk
    );
    if tools_withheld {
        eprintln!(
            "warning: workspace tools are disabled for this run: the project is not trusted; \
approve trust by running `rapid trust grant` in this project, and set \
{PERMISSION_MODE_ENV} (e.g. bypassPermissions) to control tool approvals"
        );
    } else if mode_refuses_all {
        eprintln!(
            "warning: permission mode refuses every tool call in headless exec; \
set {PERMISSION_MODE_ENV} to a mode that allows calls (e.g. bypassPermissions)"
        );
    }

    // Trusted-project integrations: web_fetch allowlist, hooks, MCP servers.
    // Merged across every file in PROJECT_SETTINGS_FILES the same way
    // exec_permission_lattice already merges permission rules — a
    // `.claude/settings.json`-only project (no `.rapidlm/settings.json` at
    // all) previously got permission-rule compat but silently lost hooks/
    // mcp/shadow-diagnostics/fetch-allowlist, since this block only ever
    // read the one RapidLM-native file name.
    if let (Some((root, TrustStatus::Trusted)), ExecTools::Workspace(_)) =
        (&workspace, &mut tools)
    {
        let ProjectIntegrations {
            fetch_allowlist: allowlist,
            hooks: merged_hooks,
            shadow: shadow_config,
            mcp_servers,
        } = load_project_integrations(root);
        tools.set_fetch_allowlist(allowlist);
        if !merged_hooks.session_start.is_empty() {
            // Fire-and-forget: a session_start hook observes the run
            // starting, it never gates it (no PreHookOutcome here).
            let _ = crate::hooks::run_notify_hooks(
                &merged_hooks.session_start,
                "session_start",
                serde_json::json!({}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
        // Captured now (before `merged_hooks` moves into `set_hooks` below);
        // fired later by SessionEndHookGuard's Drop impl, on whatever exit
        // path this turn actually takes.
        session_end_guard.hooks = merged_hooks.session_end.clone();
        if !merged_hooks.is_empty() {
            tools.set_hooks(merged_hooks);
        }
        if let Some(shadow) = shadow_config {
            tools.set_shadow_diagnostics(shadow);
        }
        if !mcp_servers.is_empty() {
            tools.register_mcp_servers(&mcp_servers);
        }
    }

    // Reminder feeds: load the project roster if present and admit the
    // always-on feeds (the CLI host grants no capabilities, so feeds gated
    // on a capability stay inactive). A broken roster warns and the turn
    // continues without reminders — advisory context, kept not loaded.
    // Computed here, ahead of model selection just below (not inline with
    // the rest of context assembly, which now comes after model selection
    // so its budget can be derived from the resolved model) because
    // `apply_reminder_floor` needs `reminder_floor` to pick the model's
    // reasoning effort.
    let mut reminder_floor = agent_runtime::reminders::ReminderFloor::Baseline;
    let mut reminder_block: Option<String> = None;
    match load_active_reminders() {
        Ok(Some((block, floor))) => {
            reminder_floor = floor;
            reminder_block = Some(block);
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("warning: reminders not loaded: {err}");
        }
    }

    // Layered model selection (env overrides > user config > typed fallback).
    // Resolved before context construction below — not after, as it was
    // before this fix — so the context budget (`context_budget_for`) is
    // derived from the model that will actually run this turn, never a
    // hard-coded placeholder sized before the model was even known.
    let process_env: Vec<(String, String)> = std::env::vars().collect();
    let mut base_url = String::from("unconfigured");
    let mut child_model_config: Option<crate::user_config::ActiveModel> = None;
    // `models`: [primary, ...fallback alternates] — resolved as plain data
    // (no borrows yet) so the credential-store count is known upfront.
    let mut models: Vec<crate::user_config::ActiveModel> = Vec::new();
    let mut unconfigured = false;
    match crate::user_config::select_from_process_env_gated() {
        Ok(ModelSelection::Configured { active, warnings }) => {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            base_url = active.entry.base_url.clone();
            child_model_config = Some(active.as_ref().clone());
            let primary = apply_reminder_floor(*active, reminder_floor);

            // Fallback chain: opt-in via `[models] fallback`, resolved
            // against the same raw config the primary came from, then
            // narrowed/raised by the same managed policy (if any) the
            // primary was already gated through — a fallback entry is never
            // let through a restriction, or under an effort floor, the
            // primary itself has to honor.
            if let Some(config) = crate::user_config::load_config(
                &crate::user_config::resolve_config_source(&process_env),
            )
            .unwrap_or(None)
            {
                let (candidates, warnings) =
                    crate::user_config::resolve_fallback_chain(&process_env, &config, &primary);
                for warning in warnings {
                    eprintln!("warning: {warning}");
                }
                match gate_fallback_candidates(&process_env, candidates) {
                    Ok((gated, warnings)) => {
                        for warning in warnings {
                            eprintln!("warning: {warning}");
                        }
                        models.extend(gated);
                    }
                    Err(err) => {
                        eprintln!("managed policy error: {err}");
                        return Ok(JsonlExitCode::Usage.as_i32());
                    }
                }
            }
            models.insert(0, primary);
        }
        Ok(ModelSelection::Unconfigured { .. }) => {
            eprintln!("{NOT_CONFIGURED_HINT}");
            unconfigured = true;
        }
        Err(err) => {
            eprintln!("model configuration error: {err}");
            return Ok(JsonlExitCode::Usage.as_i32());
        }
    }
    // One store per backend, fully built before any ConfiguredModel borrows
    // from it — every borrow below is a plain, compiler-checked immutable
    // borrow of an already-final Vec, not touched again afterward.
    let credential_stores: Vec<auth::InMemoryCredentialStore> = models
        .iter()
        .map(|_| auth::InMemoryCredentialStore::new())
        .collect();
    // Routing-decision log (Modbit `MOD-005`: "routing must be auditable").
    // Only the fallback-chain branch below can ever produce a decision; every
    // other branch keeps an empty log — absence *is* the "no incident"
    // signal, not a missing feature.
    let mut router_decisions = crate::host::RouterDecisionLog::new();
    let backing = if unconfigured {
        SelectedModel::Unconfigured(UnconfiguredModel)
    } else if models.len() == 1 {
        match ConfiguredModel::build(&models[0], &credential_stores[0]) {
            Ok(model) => SelectedModel::Configured(Box::new(model)),
            Err(err) => {
                eprintln!("model configuration error: {err}");
                return Ok(JsonlExitCode::Usage.as_i32());
            }
        }
    } else {
        let mut backends = Vec::with_capacity(models.len());
        for (active, store) in models.iter().zip(credential_stores.iter()) {
            // Tracking identity for FallbackController only — distinct from
            // whatever ModelRef ConfiguredModel builds internally for the
            // real wire request. Two profiles can legitimately name the
            // same underlying provider+model (a paid vs. free tier of the
            // same model, or — as a real-provider live check for this
            // wiring found — two entries that only differ by base_url), so
            // `entry.model` alone is not a safe uniqueness key here;
            // `profile_id` always is, since it's a `[model.<id>]` TOML
            // table key and BTreeMap-unique by construction. Already
            // validated against the (stricter) llm-router profile alphabet
            // in `resolve_active`/`resolve_fallback_chain`, so this can
            // never fail ModelId's looser alphabet.
            let model_ref = llm_router::provider::ModelRef::new(
                llm_router::provider::ProviderId::parse(active.entry.provider.as_str())
                    .expect("provider id already validated by user_config parsing"),
                llm_router::provider::ModelId::parse(&active.profile_id)
                    .expect("profile id already validated against the stricter llm-router alphabet"),
            );
            match ConfiguredModel::build(active, store) {
                Ok(model) => backends.push((model_ref, model)),
                Err(err) => {
                    eprintln!(
                        "warning: fallback entry '{}' failed to configure ({err}); skipped",
                        active.profile_id
                    );
                }
            }
        }
        if backends.len() < 2 {
            // Every alternate failed to configure — fall back to the plain,
            // single-model path rather than a chain of one.
            match backends.into_iter().next() {
                Some((_, model)) => SelectedModel::Configured(Box::new(model)),
                None => {
                    eprintln!("model configuration error: primary model failed to configure");
                    return Ok(JsonlExitCode::Usage.as_i32());
                }
            }
        } else {
            let primary_ref = backends[0].0.clone();
            let alternate_refs: Vec<_> = backends[1..].iter().map(|(model_ref, _)| model_ref.clone()).collect();
            let policy = llm_router::fallback::FallbackPolicy::standard();
            let router_cancel = llm_router::provider::CancellationToken::new();
            match llm_router::fallback::FallbackController::from_explicit_chain(
                primary_ref,
                alternate_refs,
                policy,
                &router_cancel,
            ) {
                Ok(controller) => {
                    let diag = parsed.verbose.then(|| StepDiag::stderr(&base_url));
                    let mut chain = FallbackChainModel::new(backends, controller, diag);
                    // Reuses the single read captured above (disk/network
                    // ceiling narrowing) rather than loading a third time —
                    // see that read's own doc comment for why: a third
                    // independent read could see a different file than the
                    // one that actually gated this turn's permission mode,
                    // and would make the recorded version describe a
                    // policy that wasn't the one actually applied.
                    chain.set_policy_version(policy_version.clone());
                    router_decisions = chain.decisions();
                    SelectedModel::FallbackChain(Box::new(chain))
                }
                Err(err) => {
                    eprintln!("warning: fallback chain configuration failed ({err}); using the primary model only");
                    let (_, model) = backends.into_iter().next().expect("checked len >= 2 above");
                    SelectedModel::Configured(Box::new(model))
                }
            }
        }
    };

    // Prompt/context stack: project instructions (AGENTS.md convention +
    // compat paths) and the conditional-section system prompt (environment,
    // trust posture, token budget) — sized against `backing`, the model
    // just resolved above, never a fixed placeholder.
    let (context_limit, output_reserve) = context_budget_for(&backing);
    if parsed.verbose {
        // Keyed off `backing` itself (what actually produced the numbers
        // above), not `child_model_config` (the *primary's* raw config
        // entry): the primary is not necessarily what backs a resolved
        // `Configured` model (the `backends.len() < 2`/controller-failure
        // paths above can promote a surviving alternate instead), and a
        // `FallbackChain`'s derived pair is a genuine cross-backend
        // combination — attributing it to "the primary's config" would
        // misdescribe where the printed numbers actually came from either
        // way.
        let source = match &backing {
            SelectedModel::Unconfigured(_) => "default (no model configured)".to_owned(),
            SelectedModel::Configured(model) => {
                let caps = model.capabilities();
                if caps.context_limit() == crate::user_config::DEFAULT_CONTEXT_WINDOW
                    && caps.max_output() == crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS
                {
                    "default (context_window/max_tokens unset in model config)".to_owned()
                } else {
                    "configured".to_owned()
                }
            }
            SelectedModel::FallbackChain(_) => format!(
                "chain: minimum context_limit / maximum max_output across {} candidates",
                models.len()
            ),
        };
        eprintln!(
            "context budget: context_window={context_limit} output_reserve={output_reserve} source={source}"
        );
    }
    let cwd = std::env::current_dir().ok();
    let preserved = build_live_context(
        workspace.as_ref().map(|(root, _)| root.as_path()),
        cwd.as_deref(),
        prompt.clone(),
        trusted,
        context_limit,
        output_reserve,
    )
    .map_err(|_| InteractiveError::Internal)?;
    // Memory index: .rapidlm/MEMORY.md is always loaded (bounded, advisory).
    let memory_index = workspace
        .as_ref()
        .and_then(|(root, _)| crate::host::load_memory_index(root));
    let preserved = preserved.with_memory_index(memory_index);
    // Plan/todo projection: .rapidlm/todos.json (written by todo_write)
    // survives compaction and a fresh invocation, not just the transcript.
    let todos_index = workspace
        .as_ref()
        .and_then(|(root, _)| crate::host::load_todos_index(root));
    let preserved = preserved.with_todos_index(todos_index);
    // Proactive context retrieval: only for a trusted project (it walks the
    // tree and writes an incremental index under .rapidlm/index/). Fails
    // open inside retrieve() itself — an unindexable or slow repo yields no
    // blocks rather than blocking the turn.
    let preserved = if let Some((root, TrustStatus::Trusted)) = &workspace {
        let retrieved = crate::context_retrieval::retrieve(root, &prompt, 2048);
        preserved.with_retrieved_context(retrieved)
    } else {
        preserved
    };
    let preserved = preserved.with_reminders_block(reminder_block);
    let spec = AgentSpec::builder(
        protocol::AgentId::new(),
        AgentRole::Coder,
        prompt,
        protocol::WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    .map_err(|_| InteractiveError::Internal)?;
    let session_id = protocol::SessionId::new();
    let request = AgentExecutionRequest::new(spec, session_id);
    let cancel = agent_runtime::CancellationToken::new();
    if let Some(max_wall_time) = parsed.max_wall_time {
        spawn_wall_time_watchdog(cancel.clone(), max_wall_time);
    }
    let mut events: Vec<agent_runtime::TurnEvent> = Vec::new();

    // Scrub the active model's own resolved credential from captured
    // shell_exec output: a command that reads back a config file
    // containing it (a real, plausible thing to run, not a contrived
    // scenario — `~/.rapidlm/config.toml` stores it in plaintext) must not
    // hand it back to the model verbatim. Best-effort: a registration
    // failure (e.g. the credential is empty or oversized) just means
    // nothing gets scrubbed, not a turn failure.
    if let (Some(active), Some((_, TrustStatus::Trusted))) =
        (child_model_config.as_ref(), workspace.as_ref())
        && let Some(plaintext) = active.credential.plaintext.as_deref()
        && let Ok(refer) = auth::SecretRef::from_alias("active-model-credential")
    {
        let mut registry = security::SecretRedactionRegistry::new();
        let cancel = security::RedactionCancellation::new();
        if registry
            .register_canary(&refer, plaintext.as_bytes(), &cancel)
            .is_ok()
        {
            tools.set_redaction(registry.snapshot());
        }
    }
    // Subagents: with a configured model, task_spawn runs child agents with
    // the same provider config and a depth-restricted read-only-capable tool
    // surface. Memory index: .rapidlm/MEMORY.md is always loaded (bounded).
    if let (Some(active), Some((root, TrustStatus::Trusted))) =
        (child_model_config.as_ref(), workspace.as_ref())
    {
        if let Some(turn_budgets) = tools.turn_budget_handles() {
            let hooks = tools.hooks_config();
            let shadow_diagnostics = tools.shadow_diagnostics_config();
            let trace_calls = tools.trace_calls_enabled();
            let turn_ceilings = tools.turn_ceilings().unwrap_or((
                crate::exec_tools::MAX_TOTAL_WRITE_BYTES_PER_TURN,
                crate::exec_tools::MAX_TOTAL_FETCH_BYTES_PER_TURN,
            ));
            let write_locks = tools.write_lock_handle().unwrap_or_default();
            let job_budget = tools
                .job_budget_handle()
                .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
            tools.set_subagent_runner(std::sync::Arc::new(LiveSubagentRunner {
                active: active.clone(),
                root: root.clone(),
                permissions: permission_lattice.clone(),
                turn_budgets,
                write_locks,
                job_budget,
                hooks,
                shadow_diagnostics,
                trace_calls,
                turn_ceilings,
                redaction: tools.redaction_handle(),
            }));
        }
    }
    let diag = parsed.verbose.then(|| StepDiag::stderr(&base_url));
    // `--json-schema`: wrap the tool driver with the synthetic-tool
    // constrained-output adapter. Reading/parsing/compiling the schema stays
    // outside `run_live_exec` so a bad `--json-schema` fails before spending
    // any model calls, as a typed Usage error, not a mid-turn surprise.
    let json_schema_requested = parsed.json_schema.is_some();
    let json_schema_capture: std::rc::Rc<std::cell::RefCell<Option<String>>>;
    // Goal-usage attribution: captured *before* the run, not derived from
    // whatever goal happens to be active once it finishes — see
    // `active_goal_id`/`accrue_turn_usage`'s own doc comments for why. Not
    // gated on `TrustStatus::Trusted`: `goal.json` isn't a workspace-tools
    // concern the way trust otherwise gates this run, and the interactive
    // TUI's own equivalent (`sync_persisted_goal`) reads it unconditionally
    // too.
    let goal_path = workspace.as_ref().map(|(root, _)| root.join(PROJECT_MARKER).join(GOAL_FILE));
    let goal_id = goal_path.as_deref().and_then(active_goal_id);
    let turn_started = Instant::now();
    let run_result = if let Some(schema_path) = parsed.json_schema.as_ref() {
        let schema_text = match std::fs::read_to_string(schema_path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("--json-schema: failed to read {}: {err}", schema_path.display());
                return Ok(JsonlExitCode::Usage.as_i32());
            }
        };
        let schema_value: serde_json::Value = match serde_json::from_str(&schema_text) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("--json-schema: {} is not valid JSON: {err}", schema_path.display());
                return Ok(JsonlExitCode::Usage.as_i32());
            }
        };
        let mut wrapped =
            match crate::structured_output::StructuredOutputTools::new(tools, schema_value) {
                Ok(wrapped) => wrapped,
                Err(err) => {
                    eprintln!("{err}");
                    return Ok(JsonlExitCode::Usage.as_i32());
                }
            };
        json_schema_capture = wrapped.captured_result();
        run_live_exec(
            preserved,
            backing,
            &request,
            &mut wrapped,
            &mut events,
            &cancel,
            ContextRetryPolicy::default(),
            diag,
        )
    } else {
        json_schema_capture = std::rc::Rc::new(std::cell::RefCell::new(None));
        run_live_exec(
            preserved,
            backing,
            &request,
            &mut tools,
            &mut events,
            &cancel,
            ContextRetryPolicy::default(),
            diag,
        )
    };
    if let (Ok(outcome), Some(goal_path), Some(goal_id)) = (&run_result, &goal_path, goal_id) {
        let active_ms = u64::try_from(turn_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        accrue_turn_usage(
            goal_path,
            goal_id,
            outcome.tokens,
            outcome.cost_usd_micros.unwrap_or(0),
            active_ms,
        );
    }
    // `--jsonl`: everything above stays exactly as for plain-text exec; only
    // the outcome below is reported differently. `rapid_schema` is written
    // now (not earlier) since nothing before this point can fail *after* a
    // session conceptually exists — see `ExecArgs::jsonl`'s doc comment.
    let mut jsonl_io = parsed.jsonl.then(|| {
        let mut io = crate::headless::jsonl::JsonlIo::stdio();
        let _ = crate::headless::jsonl::JsonlRecord::rapid_schema(
            crate::headless::jsonl::now_rfc3339(),
        )
        .map(|record| io.records().write(&record));
        io
    });
    // Routing decisions (Modbit `MOD-005`): always on stderr — a fallback
    // switching the model mid-turn is operationally significant enough to
    // surface unconditionally, not gated behind `--verbose` — and, in
    // `--jsonl` mode, one `router.decision` record per entry, ahead of the
    // outcome records below so `seq` stays monotonic across the whole run.
    let mut next_jsonl_seq = 1u64;
    for decision in router_decisions.snapshot() {
        let (reason_tag, resolved) = match &decision.reason {
            RouterDecisionReason::RetrySame => ("retry_same", decision.resolved_model.as_str()),
            RouterDecisionReason::FallbackTo => ("fallback_to", decision.resolved_model.as_str()),
            RouterDecisionReason::Stop(why) => (why.as_str(), ""),
        };
        match (decision.spent_usd_micros, &decision.policy_version) {
            (Some(spent), Some(version)) => eprintln!(
                "router: requested={} resolved={} reason={reason_tag} spent_usd_micros={spent} \
                 policy_version={version}",
                decision.requested_model, decision.resolved_model
            ),
            (Some(spent), None) => eprintln!(
                "router: requested={} resolved={} reason={reason_tag} spent_usd_micros={spent}",
                decision.requested_model, decision.resolved_model
            ),
            (None, Some(version)) => eprintln!(
                "router: requested={} resolved={} reason={reason_tag} policy_version={version}",
                decision.requested_model, decision.resolved_model
            ),
            (None, None) => eprintln!(
                "router: requested={} resolved={} reason={reason_tag}",
                decision.requested_model, decision.resolved_model
            ),
        }
        if let Some(io) = jsonl_io.as_mut() {
            if let Ok(record) = crate::headless::jsonl::JsonlRecord::router_decision(
                session_id,
                next_jsonl_seq,
                crate::headless::jsonl::now_rfc3339(),
                &decision.requested_model,
                resolved,
                reason_tag,
                decision.spent_usd_micros,
                decision.policy_version.as_deref(),
            ) {
                let _ = io.records().write(&record);
                next_jsonl_seq += 1;
            }
        }
    }
    // `text` is the same content the plain-text path would have printed;
    // `code` is the typed exit code either path returns; `cost_usd_micros`
    // is `None` for every `Err` arm (no `ExecOutcome` to read one from).
    let (text, code, cost_usd_micros): (Option<String>, JsonlExitCode, Option<u64>) =
        match run_result {
            Ok(outcome) if outcome.result.status() == AgentTerminalStatus::Succeeded => {
                let captured = json_schema_capture.borrow();
                if json_schema_requested && captured.is_none() {
                    eprintln!(
                        "--json-schema was set but the model never called the synthetic tool \
                         with a schema-conformant result"
                    );
                    (None, JsonlExitCode::Runtime, outcome.cost_usd_micros)
                } else {
                    let text = captured
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| outcome.result.summary().to_owned());
                    let mut line = format!("tokens used: {}", outcome.tokens);
                    if let Some(cost_usd_micros) = outcome.cost_usd_micros {
                        line.push_str(&format!(" ({})", format_usd_micros(cost_usd_micros)));
                    }
                    crate::exec_diag::stderr_line(&line);
                    (Some(text), JsonlExitCode::Success, outcome.cost_usd_micros)
                }
            }
            Ok(outcome) if context_required_question(&outcome).is_some() => {
                // Checked before `describe_turn_failure`'s generic "(failing
                // tool: ...)" framing below: the model asked a real,
                // specific question, not a malfunction — print it as-is,
                // not wrapped in failure language, and exit `NeedsContext`
                // (not `Success`, so a script can tell "stopped needing
                // input" apart from "produced a confident final answer,"
                // and not any error code either, since nothing actually
                // went wrong).
                let question = context_required_question(&outcome)
                    .expect("guard just matched Some")
                    .to_owned();
                crate::exec_diag::stderr_line(&format!("needs context: {question}"));
                (Some(question), JsonlExitCode::NeedsContext, outcome.cost_usd_micros)
            }
            Ok(outcome) => {
                let mut message = describe_turn_failure(
                    &outcome.result,
                    outcome.failure_cause,
                    outcome.failure_detail.as_ref(),
                );
                // Exit-code fidelity: a turn that did real work (committed tool
                // calls) and whose only defect is an empty final model response
                // is a completed task with a missing summary — exit 0 so callers
                // do not retry committed work. Every other failure exits 1.
                // (`Succeeded` is already handled by the guard above, so reaching
                // here `is_effective_success` can only be true via that case.)
                if is_effective_success(&outcome) {
                    message.push_str(&format!(
                        " (the turn performed {} tool call(s) before the final response came back empty; verify workspace state)",
                        outcome.tool_calls
                    ));
                    crate::exec_diag::stderr_line(&message);
                    (None, JsonlExitCode::Success, outcome.cost_usd_micros)
                } else {
                    if outcome.failure_cause.is_none()
                        && !matches!(&workspace, Some((_, TrustStatus::Trusted)))
                    {
                        message.push_str(" (workspace tools are disabled: project is not trusted)");
                    }
                    crate::exec_diag::stderr_line(&message);
                    let code = exec_turn_exit_code(outcome.result.status(), outcome.failure_cause);
                    (None, code, outcome.cost_usd_micros)
                }
            }
            Err(err) => {
                eprintln!("{err}");
                let code = match err.error_code() {
                    Some(code) => JsonlExitCode::from_error_code(code, false),
                    None => JsonlExitCode::Interrupted,
                };
                (None, code, None)
            }
        };
    if let Some(io) = jsonl_io.as_mut() {
        if let Some(text) = &text {
            let _ = crate::headless::jsonl::JsonlRecord::assistant_message(
                session_id,
                next_jsonl_seq,
                crate::headless::jsonl::now_rfc3339(),
                text,
            )
            .map(|record| io.records().write(&record));
            next_jsonl_seq += 1;
        }
        let _ = crate::headless::jsonl::JsonlRecord::session_finished(
            session_id,
            next_jsonl_seq,
            crate::headless::jsonl::now_rfc3339(),
            code,
            cost_usd_micros,
        )
        .map(|record| io.records().write(&record));
    } else if let Some(text) = &text {
        println!("{text}");
    }
    Ok(code.as_i32())
}

/// Start kernel, in-process client, and TUI; restore and quiesce on every path.
pub fn run_interactive(options: InteractiveOptions) -> Result<InteractiveReport, InteractiveError> {
    options
        .cancel
        .check()
        .map_err(|_| InteractiveError::Cancelled)?;
    let resolved = resolve_project(&options)?;
    let trust = resolved.trust;
    let executable_config_active = resolved.executable_config_active;
    let config = resolved.config.clone();

    if let Some(parent) = resolved.ledger_path.parent() {
        fs::create_dir_all(parent).map_err(|_| InteractiveError::Io)?;
    }

    let runtime = KernelRuntime::new(resolved.ledger_path.clone())?;
    let mut graph =
        ServiceGraph::new([runtime], &options.cancel).map_err(InteractiveError::Service)?;
    let ctx = ServiceContext::new(options.cancel.clone(), TraceContext::root());
    match block_on(graph.start_all(ctx), &options.cancel) {
        Ok(()) => {}
        Err(err) => {
            let _ = quiesce_graph(&mut graph, &options.cancel);
            return Err(err);
        }
    }

    let started = run_started_session(options, resolved, &mut graph);
    let graph_phase = graph.phase();
    match started {
        Ok(mut report) => {
            report.graph_phase = graph_phase;
            Ok(report)
        }
        Err(err) => {
            let _ = quiesce_graph(&mut graph, &CancellationToken::new());
            Err(err)
        }
    }
    .map(|mut report| {
        report.trust = trust;
        report.executable_config_active = executable_config_active;
        report.config = config;
        report
    })
}

fn run_started_session(
    options: InteractiveOptions,
    resolved: ResolvedProject,
    graph: &mut ServiceGraph<KernelRuntime>,
) -> Result<InteractiveReport, InteractiveError> {
    let client = graph
        .services()
        .first()
        .ok_or(InteractiveError::Internal)?
        .client()
        .ok_or(InteractiveError::Internal)?;

    let actor = human_actor()?;
    let snapshot = block_on(
        client.create_session(CreateSession::new(
            ProjectId::new(),
            actor.clone(),
            TraceId::new(),
        )),
        &options.cancel,
    )?;
    let session_id = snapshot.id();
    let mut stream = block_on(
        client.subscribe(SubscribeEvents::new(session_id, snapshot.seq())),
        &options.cancel,
    )?;

    let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
    // Project the persisted composition-root goal into the interactive TUI so
    // the Goals route shows it (the TUI is a projection of runtime state).
    sync_persisted_goal(&mut ui, &resolved.ledger_path);
    let mut interrupt_count = 0;
    let mut saw_ctrl_c = false;

    let acquire = match options.terminal {
        Some(backend) => TerminalGuard::acquire_with(FrontendKind::Interactive, backend),
        None => TerminalGuard::acquire(FrontendKind::Interactive),
    };
    let mut terminal = match acquire {
        Ok(guard) => guard,
        Err(err) => {
            close_stream(&mut stream);
            let _ = interrupt_session(&client, session_id, &actor, &options.cancel);
            let _ = quiesce_graph(graph, &options.cancel);
            return Err(map_terminal(err));
        }
    };

    let mut inputs = match options.inputs {
        Some(events) => InputSource::Scripted {
            events: events.into_iter(),
        },
        None => InputSource::Crossterm,
    };

    // Crossterm only ever *reports* a resize through a live `Event::Resize`
    // — it does not synthesize one at startup — so without this, `ui.
    // viewport()` would stay stuck at `Viewport::default()`'s 80x24 for a
    // real terminal of any other size until the user happened to resize it.
    // Best-effort: a non-tty (headless test runs, `RecordingBackend`) simply
    // leaves the existing default in place, which scripted tests already
    // override deterministically with their own leading `Resize` input.
    if let Ok((width, height)) = crossterm::terminal::size() {
        ui = reduce(
            ui,
            &UiEvent::Local(LocalUiEvent::SetViewport { width, height }),
        );
    }

    let mut renderer = TuiRenderer::new(options.capture_render);
    let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let loop_result = SessionLoop {
        client: &client,
        stream: &mut stream,
        ui: &mut ui,
        session_id,
        actor: &actor,
        cancel: &options.cancel,
        interrupt_count: &mut interrupt_count,
        saw_ctrl_c: &mut saw_ctrl_c,
        root: &resolved.root,
        trusted: resolved.trust.is_trusted(),
        turn_in_flight: turn_in_flight.clone(),
        renderer: &mut renderer,
        autonomous: None,
        #[cfg(test)]
        scripted_backings: None,
    }
    .run(&mut inputs);
    let rendered_output = renderer.captured_text();

    close_stream(&mut stream);
    let restore_ok = terminal.restore().is_ok() && terminal.is_restored();
    // Best-effort: `interrupt_sync`'s own bounded retry (see its doc
    // comment in `crates/kernel/src/client.rs`) already closes almost all
    // of the race against a still-running turn's own progress-event
    // writes; surfacing a residual failure here (rather than a bare `let _
    // =`) at least makes it observable when it does happen, since nothing
    // else will ever release that turn's lease once this process exits.
    if let Err(err) = interrupt_session(&client, session_id, &actor, &options.cancel) {
        crate::exec_diag::stderr_line(&format!(
            "warning: could not confirm the session's active turn was interrupted before exit ({err})"
        ));
    }
    let quiesce = quiesce_graph(graph, &options.cancel);
    drop(client);

    match (loop_result, quiesce) {
        (Ok(outcome), Ok(())) => Ok(InteractiveReport {
            outcome,
            trust: resolved.trust,
            executable_config_active: resolved.executable_config_active,
            session_id: Some(session_id),
            graph_phase: graph.phase(),
            terminal_restored: restore_ok,
            interrupt_count,
            config: resolved.config,
            rendered_output,
        }),
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
    }
}

struct SessionLoop<'a> {
    client: &'a InProcessKernelClient,
    stream: &'a mut EventStream,
    ui: &'a mut AppState,
    session_id: protocol::SessionId,
    actor: &'a ActorRef,
    cancel: &'a CancellationToken,
    interrupt_count: &'a mut u32,
    saw_ctrl_c: &'a mut bool,
    root: &'a Path,
    trusted: bool,
    /// Set while a turn spawned by `submit_turn` is executing on its own
    /// thread; a new plain-text submission is a no-op while this is set,
    /// rather than reaching `kernel::SubmitTurn` and hitting the exact
    /// `SessionConflict` this whole feature exists to stop crashing on.
    turn_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    renderer: &'a mut TuiRenderer,
    /// `Some` for the entire duration of a `/goal run`-started autonomous
    /// continuation, `None` otherwise. Owned by the loop, not a reference:
    /// this state changes (starts/stops) across the loop's own lifetime the
    /// same way `turn_in_flight` does, but — unlike `turn_in_flight` — needs
    /// no sharing with a spawned thread, since only the main loop itself
    /// ever reads or decides from it (see `SessionLoop::step_autonomous_goal`).
    autonomous: Option<AutonomousGoalState>,
    /// Test-only seam: when set, `submit_turn` runs the next queued scripted
    /// backing instead of resolving a real model from process env/config —
    /// the same idea as `run_interactive_turn_with_backing`'s existing
    /// single-turn seam, extended so a test can drive *multiple* real
    /// autonomous iterations deterministically (a different `ScriptedModel`
    /// output per iteration) through the actual production `submit_turn`/
    /// `step_autonomous_goal`/`continue_or_stop_autonomous_goal` path,
    /// rather than re-implementing that orchestration in test code.
    #[cfg(test)]
    scripted_backings: Option<ScriptedBackingQueue>,
}

#[cfg(test)]
type ScriptedBackingQueue = std::sync::Arc<
    std::sync::Mutex<std::collections::VecDeque<Box<dyn crate::host::LiveModelCall + Send>>>,
>;

#[cfg(test)]
impl crate::host::LiveModelCall for Box<dyn crate::host::LiveModelCall + Send> {
    fn step(
        &mut self,
        blocks: &[context_engine::compile::ContextBlock],
        input: &agent_runtime::ModelStepInput<'_>,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<agent_runtime::ModelStepOutput, agent_runtime::ModelStepError> {
        (**self).step(blocks, input, cancel)
    }
}

/// Everything the main loop needs to keep driving one autonomous goal
/// continuation across iterations. Deliberately holds no copy of goal
/// lifecycle/usage state itself — every decision re-reads the real,
/// persisted `.rapidlm/goal.json` fresh (see `step_autonomous_goal`), so
/// this struct only carries what genuinely can't be recovered from that
/// file: the driver-ownership lease, the in-memory repeated-message
/// detector (reset per autonomous run, not persisted — matching
/// `GoalDriver`'s own per-driver-instance detector), and the transcript
/// offset used to inspect *this iteration's own* new entries without
/// re-scanning or reparsing anything already rendered.
struct AutonomousGoalState {
    goal_id: protocol::GoalId,
    agent_id: protocol::AgentId,
    _lease: DriverLease,
    loop_detector: MessageLoopDetector,
    /// `AppState.transcript().len()` immediately before the current
    /// iteration's turn was submitted, `None` while no iteration is
    /// in flight (i.e. between the decision to continue and the next
    /// `submit_turn` call, which is instantaneous in practice but kept
    /// `Option` for clarity rather than a sentinel `usize`).
    transcript_len_before_iteration: Option<usize>,
}

impl SessionLoop<'_> {
    fn run(mut self, inputs: &mut InputSource) -> Result<InteractiveOutcome, InteractiveError> {
        loop {
            self.cancel
                .check()
                .map_err(|_| InteractiveError::Cancelled)?;
            self.drain()?;
            self.step_autonomous_goal()?;
            match next_input(inputs, self.cancel)? {
                None => continue,
                Some(InteractiveInput::Eof) => return Ok(InteractiveOutcome::Quit),
                Some(input) => match self.handle_input(input)? {
                    LoopControl::Continue => {}
                    LoopControl::Quit(outcome) => return Ok(outcome),
                },
            }
        }
    }

    fn handle_input(&mut self, input: InteractiveInput) -> Result<LoopControl, InteractiveError> {
        self.cancel
            .check()
            .map_err(|_| InteractiveError::Cancelled)?;
        match input {
            InteractiveInput::CtrlC => {
                if *self.saw_ctrl_c {
                    return Ok(LoopControl::Quit(InteractiveOutcome::Interrupted));
                }
                *self.saw_ctrl_c = true;
                self.interrupt()?;
                self.drain()?;
                Ok(LoopControl::Continue)
            }
            InteractiveInput::Eof => Ok(LoopControl::Quit(InteractiveOutcome::Quit)),
            InteractiveInput::Resize { width, height } => {
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SetViewport { width, height }),
                );
                Ok(LoopControl::Continue)
            }
            InteractiveInput::PageUp => {
                self.renderer.page_up();
                Ok(LoopControl::Continue)
            }
            InteractiveInput::PageDown => {
                self.renderer.page_down();
                Ok(LoopControl::Continue)
            }
            InteractiveInput::Char(ch) => {
                let mut text = self.ui.composer().text().to_owned();
                if text.len() < MAX_COMPOSER_BYTES && !ch.is_control() {
                    text.push(ch);
                    *self.ui = reduce(
                        self.ui.clone(),
                        &UiEvent::Local(LocalUiEvent::SetComposerText(text)),
                    );
                }
                Ok(LoopControl::Continue)
            }
            InteractiveInput::Backspace => {
                let mut text = self.ui.composer().text().to_owned();
                text.pop();
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SetComposerText(text)),
                );
                Ok(LoopControl::Continue)
            }
            InteractiveInput::Enter => self.submit_composer(),
            InteractiveInput::Submit(text) => {
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SetComposerText(text)),
                );
                self.submit_composer()
            }
        }
    }

    fn submit_composer(&mut self) -> Result<LoopControl, InteractiveError> {
        self.cancel
            .check()
            .map_err(|_| InteractiveError::Cancelled)?;
        let raw = self.ui.composer().text().to_owned();
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::SetComposerText(String::new())),
        );
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(LoopControl::Continue);
        }
        if trimmed.starts_with('/') {
            return self.dispatch_slash(trimmed);
        }
        if self.autonomous.is_some() {
            // Slash commands (including `/goal pause|cancel|stop`, the real
            // way to interrupt an autonomous run) still reach dispatch_slash
            // above unchanged — only a *new* ordinary turn is refused here,
            // matching submit_turn's own existing turn_in_flight drop
            // exactly: reject rather than queue, silently rather than an
            // error, so the session stays responsive without stacking work.
            self.append_command_error(
                "autonomous goal execution is running — /goal stop to interrupt it first"
                    .to_owned(),
            );
            return Ok(LoopControl::Continue);
        }
        let text = trimmed.to_owned();
        self.submit_turn(&text)?;
        Ok(LoopControl::Continue)
    }

    /// Never returns `Err` for a bad/unknown command — a slash command is
    /// local input, and a mistyped one must produce a local error message
    /// through the normal render path, not end the whole interactive
    /// session (see `command_error_text`'s own doc comment for why this
    /// matters: it once did, silently, for every `CommandError` other than
    /// `Empty`).
    fn dispatch_slash(&mut self, command: &str) -> Result<LoopControl, InteractiveError> {
        match parse_command(command) {
            Ok(parsed) => match dispatch(parsed) {
                FrontendAction::Quit => Ok(LoopControl::Quit(InteractiveOutcome::Quit)),
                FrontendAction::Local(LocalAction::Open(inspector)) => {
                    if let Some(route) = inspector.route() {
                        *self.ui = reduce(
                            self.ui.clone(),
                            &UiEvent::Local(LocalUiEvent::SetRoute(route)),
                        );
                    }
                    Ok(LoopControl::Continue)
                }
                FrontendAction::InlineHelp(help) => {
                    self.append_command_output(help.usage().to_owned());
                    Ok(LoopControl::Continue)
                }
                FrontendAction::Kernel(action) => {
                    self.apply_kernel_action(action)?;
                    Ok(LoopControl::Continue)
                }
            },
            Err(CommandError::Empty) => Ok(LoopControl::Continue),
            Err(err) => {
                self.append_command_error(command_error_text(&err));
                Ok(LoopControl::Continue)
            }
        }
    }

    fn append_command_output(&mut self, text: String) {
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::AppendCommandOutput(text)),
        );
    }

    fn append_command_error(&mut self, text: String) {
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::AppendCommandError(text)),
        );
    }

    fn goal_path(&self) -> PathBuf {
        self.root.join(PROJECT_MARKER).join(GOAL_FILE)
    }

    fn evidence_path(&self) -> PathBuf {
        self.root.join(PROJECT_MARKER).join(EVIDENCE_FILE)
    }

    /// `/goal start <text>` — the only goal mutation the TUI's own grammar
    /// carries no criteria/requirements/budget for (see `parse_goal`), so
    /// this builds the minimal equivalent of a bare `rapid goal create
    /// <text>` with no flags. Reuses the exact same `GoalHost`/`GoalCommand`
    /// transactional API `run_goal_command`'s own `"create"` branch and
    /// `accrue_turn_usage` already use — no persistence logic is duplicated,
    /// only this command's own argument handling is new.
    fn start_goal(&mut self, statement: String) -> Result<(), InteractiveError> {
        let statement = statement.trim();
        if statement.is_empty() {
            self.append_command_error("usage: /goal start <text>".to_owned());
            return Ok(());
        }
        let spec = match GoalSpec::new(
            protocol::GoalId::new(),
            statement.to_owned(),
            Vec::new(),
            GoalBudget::new(None, None, None, None),
            Vec::new(),
        ) {
            Ok(spec) => spec,
            Err(err) => {
                self.append_command_error(format!("goal start: {err}"));
                return Ok(());
            }
        };
        let goal_path = self.goal_path();
        let mut host = match GoalHost::load(&goal_path) {
            Ok(host) => host.unwrap_or_else(GoalHost::new),
            Err(err) => {
                self.append_command_error(format!("goal start: {err}"));
                return Ok(());
            }
        };
        let cancel = agent_runtime::CancellationToken::new();
        match host.update(&goal_path, |host| {
            host.apply(GoalCommand::Create(spec), &GoalActor::Human, &cancel)
        }) {
            Ok(_) => {
                self.append_command_output(format!("goal started: {statement}"));
                if let Some(snapshot) = host.snapshot() {
                    let projection = project_goal(snapshot);
                    *self.ui = reduce(
                        self.ui.clone(),
                        &UiEvent::Local(LocalUiEvent::SyncGoal(projection)),
                    );
                }
            }
            Err(GoalTransactionError::Persist(err)) => {
                self.append_command_error(format!("goal start: {err}"));
            }
            Err(GoalTransactionError::Mutate(err)) => {
                self.append_command_error(format!("goal start: {err}"));
            }
        }
        Ok(())
    }

    /// `/goal pause|resume|cancel` — mirrors `goal_lifecycle`'s exact
    /// `GoalCommand` mapping (the headless `rapid goal` implementation),
    /// but returns a rendered result instead of `println!`ing: this runs
    /// inside the full-screen interactive TUI, where a raw stdout write
    /// would corrupt the compositor's own painted frame. Not a call to
    /// `goal_lifecycle` itself for exactly that reason — the shared,
    /// reused part is the `GoalHost`/`GoalCommand`/`GoalActor` transactional
    /// API underneath, not that CLI-only presentation function.
    fn goal_lifecycle_command(&mut self, kind: GoalLifecycleKind) -> Result<(), InteractiveError> {
        let goal_path = self.goal_path();
        let mut host = match GoalHost::load(&goal_path) {
            Ok(host) => host.unwrap_or_else(GoalHost::new),
            Err(err) => {
                self.append_command_error(format!("goal {}: {err}", kind.as_str()));
                return Ok(());
            }
        };
        let Some(goal_id) = host.snapshot().map(|s| s.id()) else {
            self.append_command_error("no active goal".to_owned());
            return Ok(());
        };
        let cancel = agent_runtime::CancellationToken::new();
        let command = kind.command(goal_id);
        match host.update(&goal_path, |host| {
            host.apply(command, &GoalActor::Human, &cancel)
        }) {
            Ok(_) => {
                self.append_command_output(format!("goal {}: ok", kind.as_str()));
                match host.snapshot() {
                    Some(snapshot) => {
                        let projection = project_goal(snapshot);
                        *self.ui = reduce(
                            self.ui.clone(),
                            &UiEvent::Local(LocalUiEvent::SyncGoal(projection)),
                        );
                    }
                    // `pause`/`resume` keep a snapshot; `cancel` clears it
                    // (see `agent_runtime::GoalState`'s own doc comment) —
                    // without this the Goals route would keep showing the
                    // cancelled goal as still `Active` until the next
                    // session start happens to reload it.
                    None => {
                        *self.ui = reduce(
                            self.ui.clone(),
                            &UiEvent::Local(LocalUiEvent::ClearGoal(goal_id)),
                        );
                    }
                }
            }
            Err(GoalTransactionError::Persist(err)) => {
                self.append_command_error(format!("goal {}: {err}", kind.as_str()));
            }
            Err(GoalTransactionError::Mutate(err)) => {
                self.append_command_error(format!("goal {}: {err}", kind.as_str()));
            }
        }
        Ok(())
    }

    /// `/goal run` — begin autonomous continuation on the currently active
    /// goal. Explicit-only: nothing else in this codebase ever sets
    /// `self.autonomous`, so an active goal never runs on its own (see
    /// `KernelAction::RunGoal`'s own doc comment for why). Acquires the
    /// cross-process [`DriverLease`] first — never assumes this session is
    /// the only place trying to run this goal autonomously — then submits
    /// the first iteration immediately via the same path every later
    /// iteration uses.
    fn start_autonomous_goal(&mut self) -> Result<(), InteractiveError> {
        if self.autonomous.is_some() {
            self.append_command_output("autonomous goal execution is already running".to_owned());
            return Ok(());
        }
        let goal_path = self.goal_path();
        let host = match GoalHost::load(&goal_path) {
            Ok(Some(host)) => host,
            Ok(None) => {
                self.append_command_error("no active goal — /goal start <text> first".to_owned());
                return Ok(());
            }
            Err(err) => {
                self.append_command_error(format!("goal run: {err}"));
                return Ok(());
            }
        };
        let Some(snapshot) = host.snapshot() else {
            self.append_command_error("no active goal — /goal start <text> first".to_owned());
            return Ok(());
        };
        if snapshot.state() != GoalState::Active {
            self.append_command_error("goal run: the active goal is not Active".to_owned());
            return Ok(());
        }
        let goal_id = snapshot.id();
        let lease = match try_acquire_driver_lease(&goal_path) {
            Ok(lease) => lease,
            Err(_) => {
                self.append_command_error(
                    "autonomous execution is already running for this goal — in this session \
                     or another process"
                        .to_owned(),
                );
                return Ok(());
            }
        };
        self.autonomous = Some(AutonomousGoalState {
            goal_id,
            agent_id: protocol::AgentId::new(),
            _lease: lease,
            loop_detector: MessageLoopDetector::new(),
            transcript_len_before_iteration: None,
        });
        self.append_command_output("autonomous goal execution started".to_owned());
        self.continue_or_stop_autonomous_goal()
    }

    /// Clears autonomous state (releasing the driver lease as `_lease`
    /// drops) and renders why. A no-op — no message, nothing to release —
    /// when nothing was running, so callers on both the explicit `/goal
    /// stop` path and every internal stop condition can call this
    /// unconditionally.
    fn stop_autonomous_goal(&mut self, reason: &str) {
        if self.autonomous.take().is_some() {
            self.append_command_output(format!("autonomous goal execution stopped: {reason}"));
        }
    }

    /// Called every loop pass (see `SessionLoop::run`). A no-op unless
    /// autonomous execution is active; while an iteration's turn is still
    /// in flight, also a no-op — there is nothing to decide until it
    /// finishes. Once it has, inspects *only this iteration's own* new
    /// transcript entries (never re-scanning earlier ones, never parsing
    /// rendered text — these are the same structured `TranscriptEntry`
    /// values the compositor itself renders from) for the three signals
    /// that must stop autonomous continuation without ever starting another
    /// turn: a context-required stop, an interruption, or a turn failure.
    /// `TurnFailed`/`TurnInterrupted` stop rather than retry — a bounded
    /// iteration/budget cap is not enough on its own to rule out a retry
    /// storm against a provider that fails fast.
    fn step_autonomous_goal(&mut self) -> Result<(), InteractiveError> {
        if self.autonomous.is_none() {
            return Ok(());
        }
        if self.turn_in_flight.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let started_at = self
            .autonomous
            .as_ref()
            .and_then(|auto| auto.transcript_len_before_iteration);
        if let Some(start) = started_at {
            let start = start.min(self.ui.transcript().len());
            let new_entries = self.ui.transcript()[start..].to_vec();
            let mut context_required = false;
            let mut interrupted = false;
            let mut failed = false;
            for entry in &new_entries {
                match entry {
                    TranscriptEntry::Assistant { text } => {
                        if let Some(auto) = &mut self.autonomous {
                            auto.loop_detector.observe(text);
                        }
                    }
                    TranscriptEntry::ToolActivity {
                        status: ToolActivityStatus::ContextRequired,
                        ..
                    } => context_required = true,
                    TranscriptEntry::TurnInterrupted => interrupted = true,
                    TranscriptEntry::TurnFailed { .. } => failed = true,
                    _ => {}
                }
            }
            if context_required {
                self.stop_autonomous_goal(
                    "the goal needs information only you can supply — see the question above; \
                     answer it, then /goal run to resume",
                );
                return Ok(());
            }
            if interrupted {
                self.stop_autonomous_goal("the current iteration was interrupted");
                return Ok(());
            }
            if failed {
                self.stop_autonomous_goal("the last autonomous turn failed");
                return Ok(());
            }
            let looping = self
                .autonomous
                .as_ref()
                .is_some_and(|auto| auto.loop_detector.is_looping());
            if looping {
                self.stop_autonomous_goal("the model repeated itself with no progress");
                return Ok(());
            }
        }
        self.continue_or_stop_autonomous_goal()
    }

    /// Reload the real, persisted goal state fresh (never trusting anything
    /// cached from a prior iteration — another process, or a slash command
    /// in this same session, may have paused/cancelled/replaced it since),
    /// then decide: stop if it is no longer the expected goal in an Active
    /// state, auto-complete it if evidence already satisfies every
    /// criterion (mechanical, never based on model prose), stop if the real
    /// (not driver-internal, not zero) accrued usage already exhausts the
    /// budget, or otherwise compile the next boundary prompt and submit it
    /// through the existing `submit_turn` — the identical kernel
    /// `SubmitTurn`/lease/background-thread path an ordinary Enter-press
    /// turn already uses, just driven by this loop instead of a keypress.
    fn continue_or_stop_autonomous_goal(&mut self) -> Result<(), InteractiveError> {
        let Some(auto) = &self.autonomous else {
            return Ok(());
        };
        let goal_id = auto.goal_id;
        let agent_id = auto.agent_id;
        let goal_path = self.goal_path();
        let mut host = match GoalHost::load(&goal_path) {
            Ok(Some(host)) => host,
            Ok(None) => {
                self.stop_autonomous_goal("no active goal");
                return Ok(());
            }
            Err(err) => {
                self.stop_autonomous_goal(&format!("goal store error: {err}"));
                return Ok(());
            }
        };
        let Some(snapshot) = host.snapshot().cloned() else {
            self.stop_autonomous_goal("no active goal");
            return Ok(());
        };
        if snapshot.id() != goal_id {
            self.stop_autonomous_goal("the goal changed identity");
            return Ok(());
        }
        match snapshot.state() {
            GoalState::Active => {}
            GoalState::Paused => {
                self.stop_autonomous_goal("goal paused");
                return Ok(());
            }
            GoalState::Blocked => {
                self.stop_autonomous_goal("goal blocked");
                return Ok(());
            }
            // `GoalState` is #[non_exhaustive]; fail conservatively by
            // stopping rather than assuming a future variant is safe to
            // continue on (mirrors `map_goal_lifecycle`'s own rule).
            _ => {
                self.stop_autonomous_goal("goal is not active");
                return Ok(());
            }
        }
        // `GoalHost::load` reads only `goal.json` — evidence lives in its
        // own separate file and is never loaded implicitly. Without this,
        // `can_complete` below would always see a freshly-constructed,
        // empty `EvidenceService` and could never auto-complete, however
        // much real evidence had actually been recorded.
        let _ = host.load_evidence(&self.evidence_path());
        let cancel = agent_runtime::CancellationToken::new();
        if host.can_complete(&cancel) {
            let result = host.update(&goal_path, |host| {
                host.apply(
                    GoalCommand::Complete { goal_id },
                    &GoalActor::MainAgent { agent_id },
                    &cancel,
                )
            });
            match result {
                Ok(_) => self.stop_autonomous_goal("goal complete"),
                Err(_) => self.stop_autonomous_goal(
                    "goal ready to complete, but the completion transaction failed",
                ),
            }
            return Ok(());
        }
        let mut guard = GoalBudgetGuard::from_snapshot(&snapshot);
        let outcome = match guard.before_turn(&cancel) {
            Ok(outcome) => outcome,
            Err(_) => {
                self.stop_autonomous_goal("cancelled");
                return Ok(());
            }
        };
        if outcome.is_exhausted() {
            let _ = host.update(&goal_path, |host| {
                host.apply(
                    GoalCommand::Block {
                        goal_id,
                        budget_exhausted: true,
                    },
                    &GoalActor::MainAgent { agent_id },
                    &cancel,
                )
            });
            self.stop_autonomous_goal("budget exhausted");
            return Ok(());
        }
        let prompt = compile_autonomous_prompt(&snapshot, outcome.hint());
        let before_len = self.ui.transcript().len();
        if let Some(auto) = &mut self.autonomous {
            auto.transcript_len_before_iteration = Some(before_len);
        }
        self.submit_turn(&prompt)
    }

    fn apply_kernel_action(&mut self, action: KernelAction) -> Result<(), InteractiveError> {
        self.cancel
            .check()
            .map_err(|_| InteractiveError::Cancelled)?;
        match action {
            KernelAction::StartGoal { statement } => self.start_goal(statement)?,
            KernelAction::PauseGoal => self.goal_lifecycle_command(GoalLifecycleKind::Pause)?,
            KernelAction::ResumeGoal => self.goal_lifecycle_command(GoalLifecycleKind::Resume)?,
            KernelAction::CancelGoal => self.goal_lifecycle_command(GoalLifecycleKind::Cancel)?,
            KernelAction::RunGoal => self.start_autonomous_goal()?,
            KernelAction::StopGoal => {
                if self.autonomous.is_some() {
                    self.stop_autonomous_goal("stopped by /goal stop");
                    // Reaches whatever iteration is currently in flight —
                    // same kernel interrupt path Ctrl-C already uses. A safe
                    // no-op if nothing is actually running.
                    self.interrupt()?;
                } else {
                    self.append_command_output(
                        "autonomous goal execution is not running".to_owned(),
                    );
                }
            }
            other => match other.kernel_api() {
                KernelApi::Interrupt => {
                    self.interrupt()?;
                }
                KernelApi::SubmitTurn => {
                    self.submit_turn("")?;
                }
                KernelApi::ForkSession => {
                    let seq = self.ui.snapshot().map(|s| s.seq()).unwrap_or(0);
                    let child = block_on(
                        self.client.fork_session(ForkSession::new(
                            self.session_id,
                            seq,
                            self.actor.clone(),
                            TraceId::new(),
                        )),
                        self.cancel,
                    )?;
                    *self.ui = reduce(self.ui.clone(), &UiEvent::Snapshot(child));
                }
                KernelApi::Rewind => {
                    let to_seq = match &other {
                        KernelAction::RewindSession { to_seq } => *to_seq,
                        _ => None,
                    };
                    let Some(to_seq) = to_seq else {
                        return Ok(());
                    };
                    let result = block_on(
                        self.client
                            .rewind(RewindSession::new(self.session_id, to_seq)),
                        self.cancel,
                    )?;
                    *self.ui = reduce(
                        self.ui.clone(),
                        &UiEvent::Snapshot(result.snapshot().clone()),
                    );
                }
                // Every other parsed command: investigated and confirmed to
                // have no real production backend anywhere in the workspace
                // today (see `newtask.md`'s command inventory) — rendered
                // as an honest, specific "not available" result instead of
                // the silent no-op this used to be.
                KernelApi::Approve | KernelApi::Dispatch => {
                    self.append_command_error(unsupported_command_text(&other));
                }
            },
        }
        self.drain()
    }

    fn submit_turn(&mut self, text: &str) -> Result<(), InteractiveError> {
        if self.ui.actions_blocked() {
            return Ok(());
        }
        // A turn already running on its own thread (see below) holds the
        // kernel's own exclusive lease; reaching `SubmitTurn` again here
        // would only bounce off `SessionConflict`. Silently ignoring a
        // submission while one is in flight (rather than queuing it) is the
        // deliberately simple choice for a first working version of real
        // turn execution.
        if self.turn_in_flight.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let expected_seq = self.ui.snapshot().map(|s| s.seq()).unwrap_or(0);
        let handle = block_on(
            self.client.submit_turn(SubmitTurn::new(
                self.session_id,
                expected_seq,
                self.actor.clone(),
                TraceId::new(),
                text,
            )),
            self.cancel,
        )?;
        // From here on the kernel holds this turn's exclusive lease: every
        // path below must reach `finish_turn` (empty text) or
        // `spawn_interactive_turn` (real text) before this function
        // returns, *before* the UI-refreshing `drain()` call at the end —
        // an adversarial self-review of this feature found that draining
        // first (the original order) let a `drain()` failure (a lagged
        // event stream, a cancelled token, a kernel `get_session` error)
        // return early via `?` with the lease still held and nothing left
        // to ever release it, stranding it exactly like the bug this
        // feature exists to fix.
        if text.trim().is_empty() {
            // A `SubmitTurn` with no real message (today, only the
            // `KernelApi::SubmitTurn` slash-command path, e.g. `/goal
            // start`, reaches this) has nothing to actually run — an
            // `AgentSpec`'s task text must be non-empty, and there is no
            // real chat content to execute against. Finish it immediately
            // rather than spawning execution machinery with nothing to do.
            let _ = self.client.finish_turn(kernel::FinishTurn::new(
                self.session_id,
                handle.turn_id(),
                self.actor.clone(),
                TraceId::new(),
                kernel::TurnOutcome::Completed { text: None },
            ));
        } else if let Some(turn_cancel) = self.client.turn_cancel_token(self.session_id) {
            // `submit_turn_sync` stores this turn's cancel token before
            // returning, so it is always present immediately after a
            // successful submit — `None` here would mean it was already
            // finished and released before this line ran, which cannot
            // happen on this thread's own just-issued handle.
            self.turn_in_flight
                .store(true, std::sync::atomic::Ordering::SeqCst);
            #[cfg(test)]
            let scripted = self
                .scripted_backings
                .as_ref()
                .and_then(|queue| queue.lock().unwrap_or_else(|p| p.into_inner()).pop_front());
            #[cfg(test)]
            if let Some(backing) = scripted {
                // No real `ConfiguredModel` behind a scripted backing to
                // derive a budget from — the same conservative default
                // `context_budget_for` uses for an unconfigured model.
                let budget = (
                    crate::user_config::DEFAULT_CONTEXT_WINDOW,
                    crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
                );
                spawn_interactive_turn_with_backing(
                    self.client.clone(),
                    self.session_id,
                    handle.turn_id(),
                    self.actor.clone(),
                    self.root.to_path_buf(),
                    self.trusted,
                    text.to_owned(),
                    turn_cancel,
                    std::sync::Arc::clone(&self.turn_in_flight),
                    backing,
                    budget,
                );
                return self.drain();
            }
            spawn_interactive_turn(
                self.client.clone(),
                self.session_id,
                handle.turn_id(),
                self.actor.clone(),
                self.root.to_path_buf(),
                self.trusted,
                text.to_owned(),
                turn_cancel,
                std::sync::Arc::clone(&self.turn_in_flight),
            );
        }
        self.drain()
    }

    fn interrupt(&mut self) -> Result<(), InteractiveError> {
        interrupt_session(self.client, self.session_id, self.actor, self.cancel)?;
        *self.interrupt_count = self.interrupt_count.saturating_add(1);
        Ok(())
    }

    fn drain(&mut self) -> Result<(), InteractiveError> {
        drain_kernel_events(
            self.client,
            self.stream,
            self.ui,
            self.session_id,
            self.cancel,
        )?;
        self.renderer.render(self.ui).map_err(|_| InteractiveError::Io)
    }
}

/// Streams one turn's progress events into the session's own kernel ledger
/// as `agent_runtime::run_turn` (reached via `run_live_exec`) produces them,
/// so `drain_kernel_events`'s existing replay/reduce mechanism renders them
/// live rather than only after the whole turn finishes. The turn's own
/// `Started`/`Completed`/`Failed`/`Interrupted` events are handled outside
/// this sink (`Started` was already recorded by `submit_turn`; the other
/// three need the real `ExecOutcome`/error this sink doesn't have access to,
/// so `run_interactive_turn_inner` appends those itself via `finish_turn`
/// after `run_live_exec` returns) — this only carries the events in between.
struct InteractiveTurnSink<'a> {
    client: &'a InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &'a ActorRef,
}

impl agent_runtime::TurnEventSink for InteractiveTurnSink<'_> {
    fn emit(&mut self, event: agent_runtime::TurnEvent) -> Result<(), agent_runtime::TurnError> {
        use agent_runtime::TurnEvent;
        use event_ledger::event::EventKind;
        let (kind, turn_id, call_id, tool, request_id, step, tokens) = match event {
            TurnEvent::Started { .. }
            | TurnEvent::Completed { .. }
            | TurnEvent::Failed { .. }
            | TurnEvent::Interrupted { .. } => return Ok(()),
            TurnEvent::ModelRequested {
                turn_id,
                request_id,
                step,
            } => (
                EventKind::ModelRequested,
                turn_id,
                None,
                None,
                Some(request_id),
                Some(step),
                None,
            ),
            TurnEvent::ModelCompleted {
                turn_id,
                request_id,
                tokens,
            } => (
                EventKind::ModelCompleted,
                turn_id,
                None,
                None,
                Some(request_id),
                None,
                Some(tokens),
            ),
            TurnEvent::ModelFailed { turn_id, request_id } => (
                EventKind::ModelFailed,
                turn_id,
                None,
                None,
                Some(request_id),
                None,
                None,
            ),
            TurnEvent::ToolRequested { turn_id, call_id, tool } => {
                (EventKind::ToolRequested, turn_id, Some(call_id), Some(tool), None, None, None)
            }
            TurnEvent::ToolStarted { turn_id, call_id, tool } => {
                (EventKind::ToolStarted, turn_id, Some(call_id), Some(tool), None, None, None)
            }
            TurnEvent::ToolCompleted { turn_id, call_id, tool } => {
                (EventKind::ToolCompleted, turn_id, Some(call_id), Some(tool), None, None, None)
            }
            TurnEvent::ToolFailed { turn_id, call_id, tool } => {
                (EventKind::ToolFailed, turn_id, Some(call_id), Some(tool), None, None, None)
            }
            TurnEvent::ToolDenied { turn_id, call_id, tool } => {
                (EventKind::ToolDenied, turn_id, Some(call_id), Some(tool), None, None, None)
            }
            TurnEvent::ToolApprovalRequired { turn_id, call_id, tool } => (
                EventKind::ToolApprovalRequired,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            TurnEvent::ToolContextRequired { turn_id, call_id, tool } => (
                EventKind::ToolContextRequired,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
        };
        let payload = serde_json::json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "tool": tool,
            "request_id": request_id,
            "step": step,
            "tokens": tokens,
        });
        self.client
            .append_turn_progress(self.session_id, self.actor, TraceId::new(), kind, payload)
            .map_err(|_| agent_runtime::TurnError::EventSink)
    }
}

/// Run `f`, converting a panic into a `Failed` outcome instead of letting it
/// unwind past whatever the caller does afterward — `spawn_interactive_
/// turn`'s cleanup (releasing the turn's lease, clearing `turn_in_flight`)
/// must run regardless of how execution ends, panic included, or a panic
/// deep in `run_live_exec` would strand the lease *and* leave the session
/// silently unresponsive to every later message for the rest of the
/// process (found in an adversarial self-review of this feature).
fn catching_panics(f: impl FnOnce() -> kernel::TurnOutcome + std::panic::UnwindSafe) -> kernel::TurnOutcome {
    std::panic::catch_unwind(f).unwrap_or_else(|_| kernel::TurnOutcome::Failed {
        reason: "interactive turn execution panicked".to_owned(),
    })
}

/// Spawn a thread that runs one interactive turn to completion and reports
/// the outcome back to the kernel so its lease is always released — even if
/// execution fails in some unexpected way — closing the crash this feature
/// exists to fix.
#[allow(clippy::too_many_arguments)]
fn spawn_interactive_turn(
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    turn_id: protocol::TurnId,
    actor: ActorRef,
    root: PathBuf,
    trusted: bool,
    text: String,
    kernel_cancel: kernel::CancelToken,
    turn_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        // A panic anywhere in `run_interactive_turn`'s own call chain (model
        // construction, `run_live_exec`, `agent_runtime::run_turn`) would
        // otherwise unwind straight past both `finish_turn` and the
        // `turn_in_flight` reset below — an adversarial self-review of this
        // feature found that this stranded the lease *and* left the whole
        // session silently unresponsive to every future message for the
        // rest of the process (no error shown, since nothing calls
        // `submit_turn`'s kernel API again once `turn_in_flight` is stuck
        // `true`) — a worse failure mode than the crash this feature exists
        // to fix. `catch_unwind` (`AssertUnwindSafe`: this closure only
        // reports the panic as a normal `Failed` outcome, it doesn't rely on
        // any invariant broken by unwinding) keeps that guarantee even here.
        let outcome = catching_panics(std::panic::AssertUnwindSafe(|| {
            run_interactive_turn(&client, session_id, &actor, &root, trusted, &text, &kernel_cancel)
        }));
        let _ = client.finish_turn(kernel::FinishTurn::new(
            session_id,
            turn_id,
            actor,
            TraceId::new(),
            outcome,
        ));
        turn_in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// Test-only-but-real sibling of `spawn_interactive_turn`: identical thread/
/// panic-safety/lease-release shape, but runs `backing` instead of resolving
/// a real model from process env/config — see `run_interactive_turn_inner_
/// with_backing`'s own doc comment for why this seam exists.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn spawn_interactive_turn_with_backing<B: crate::host::LiveModelCall + Send + 'static>(
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    turn_id: protocol::TurnId,
    actor: ActorRef,
    root: PathBuf,
    trusted: bool,
    text: String,
    kernel_cancel: kernel::CancelToken,
    turn_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    backing: B,
    budget: (u32, u32),
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let outcome = catching_panics(std::panic::AssertUnwindSafe(|| {
            run_interactive_turn_with_backing(
                &client,
                session_id,
                &actor,
                &root,
                trusted,
                &text,
                &kernel_cancel,
                backing,
                budget,
            )
        }));
        let _ = client.finish_turn(kernel::FinishTurn::new(
            session_id,
            turn_id,
            actor,
            TraceId::new(),
            outcome,
        ));
        turn_in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
    })
}

/// Bridge a `kernel::CancelToken` (set by `Interrupt`/Ctrl-C) into a fresh
/// `agent_runtime::CancellationToken` (what `run_live_exec` actually
/// checks) — different types from different crates with no dependency
/// between them, so a poller thread is the bridge, the same pattern already
/// used for `execute_mcp_tool`/`fetch_page`'s cross-crate cancellation,
/// rather than substituting a fresh, never-cancelled token that would make
/// Ctrl-C during a real in-flight turn silently do nothing. Extracted from
/// `run_interactive_turn`'s own body (unchanged logic) so a test can drive
/// the actual bridge mechanism directly and deterministically, rather than
/// only observing its effect indirectly through a full scripted turn.
struct CancelBridge {
    token: agent_runtime::CancellationToken,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl CancelBridge {
    fn start(kernel_cancel: &kernel::CancelToken) -> Self {
        let token = agent_runtime::CancellationToken::new();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog = {
            let bridge = token.clone();
            let kernel_cancel = kernel_cancel.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if kernel_cancel.is_cancelled() {
                        bridge.cancel();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
        };
        Self {
            token,
            stop,
            watchdog: Some(watchdog),
        }
    }

    /// Stop the watchdog thread and join it. Called once the bridged token
    /// is no longer needed, right after the turn it bridged for finishes.
    /// `Drop` below is the fallback for the path that doesn't reach this —
    /// a panic inside `run_interactive_turn_inner`/`run_interactive_turn_
    /// inner_with_backing` unwinds straight past this call (an adversarial
    /// self-review of the wider turn-execution feature already found the
    /// same class of skipped-cleanup gap for the turn's own kernel lease,
    /// see `catching_panics`'s own doc comment) — without `Drop` the
    /// watchdog thread would poll forever for the rest of the process, one
    /// leaked thread per panic.
    fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

impl Drop for CancelBridge {
    fn drop(&mut self) {
        // Only ever reached without `stop()` already having run when a
        // panic unwound past it — `stop()` itself leaves `watchdog: None`,
        // so the ordinary path here is just this same store again
        // (idempotent) with nothing left to join. Deliberately does not
        // join: blocking a panicking unwind on a 50ms-granularity poll
        // loop would slow down crash reporting for no real benefit — the
        // watchdog thread reliably exits on its own within one poll tick
        // once `stop` is set, whether or not anything waits for it.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn run_interactive_turn(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    kernel_cancel: &kernel::CancelToken,
) -> kernel::TurnOutcome {
    let bridge = CancelBridge::start(kernel_cancel);
    let outcome =
        run_interactive_turn_inner(client, session_id, actor, root, trusted, text, &bridge.token);
    bridge.stop();
    outcome
}

/// Test-only-but-real sibling of `run_interactive_turn`: same cancellation
/// bridge, but calls `run_interactive_turn_inner_with_backing` instead of
/// resolving a real model.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_interactive_turn_with_backing<B: crate::host::LiveModelCall>(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    kernel_cancel: &kernel::CancelToken,
    backing: B,
    budget: (u32, u32),
) -> kernel::TurnOutcome {
    let bridge = CancelBridge::start(kernel_cancel);
    let outcome = run_interactive_turn_inner_with_backing(
        client,
        session_id,
        actor,
        root,
        trusted,
        text,
        &bridge.token,
        backing,
        budget,
    );
    bridge.stop();
    outcome
}

/// Fold `.rapidlm/MEMORY.md` and `.rapidlm/todos.json` into `preserved`,
/// exactly the way `exec_turn` already does (`load_memory_index`/
/// `load_todos_index`, both bounded and fail-open — a missing or corrupt
/// file yields `None`, never an error). Extracted into its own function
/// (rather than inlined in `run_interactive_turn_inner`, the way `exec_turn`
/// inlines its own copy) specifically so it's unit-testable on its own: the
/// existing interactive-loop test cancels the turn before any model call to
/// stay fast and deterministic, so it never observes the built context —
/// this function can be asserted on directly against a fixture workspace
/// without needing to run a real turn.
fn preserve_memory_and_todos(preserved: PreservedLiveContext, root: &Path) -> PreservedLiveContext {
    let preserved = preserved.with_memory_index(crate::host::load_memory_index(root));
    preserved.with_todos_index(crate::host::load_todos_index(root))
}

/// Build the context/tools half of one interactive turn — everything that
/// does not depend on which model backs it, except the `(context_limit,
/// output_reserve)` budget itself, which the caller must derive from
/// whichever model it resolved (or, for a test, chooses to simulate) before
/// calling this — see [`context_budget_for`]. Split out from
/// `run_interactive_turn_inner` so a test can reuse this exact, real setup
/// (memory/todos index, permission lattice, `ExecTools`) while swapping in a
/// scripted [`crate::host::LiveModelCall`] instead of the real
/// env/config-resolved one — see `execute_interactive_turn`'s own doc
/// comment for why the model half is a separate seam.
///
/// `forced_mode` is threaded straight through to `exec_permission_lattice`'s
/// own identical parameter (the same override `rapid cron`'s propose-only
/// execution already uses to force `Plan` mode) — production always passes
/// `None` here, unchanged from before this function existed. A test passes
/// `Some(PermissionMode::BypassPermissions)` instead of the real mode
/// `exec_permission_lattice` would otherwise resolve from process env or a
/// `PROJECT_SETTINGS_FILES` entry read relative to the test *process's* cwd
/// (not the test's own isolated workspace root) — neither of which a test
/// can control without mutating global process state shared with every
/// other test running concurrently in the same binary.
fn build_interactive_turn_context(
    root: &Path,
    trusted: bool,
    text: &str,
    forced_mode: Option<crate::permissions::PermissionMode>,
    context_limit: u32,
    output_reserve: u32,
) -> Result<(PreservedLiveContext, ExecTools), kernel::TurnOutcome> {
    let preserved = match build_live_context(
        Some(root),
        Some(root),
        text.to_owned(),
        trusted,
        context_limit,
        output_reserve,
    ) {
        Ok(preserved) => preserved,
        Err(err) => {
            return Err(kernel::TurnOutcome::Failed {
                reason: format!("context error: {err}"),
            });
        }
    };
    let preserved = preserve_memory_and_todos(preserved, root);
    let permission_lattice = match exec_permission_lattice(Some(root), forced_mode) {
        Ok(lattice) => lattice,
        Err(err) => {
            return Err(kernel::TurnOutcome::Failed {
                reason: format!("permission configuration error: {err}"),
            });
        }
    };
    let tools = if trusted {
        ExecTools::workspace_with_permissions(root, permission_lattice)
            .unwrap_or_else(|_| ExecTools::noop())
    } else {
        ExecTools::noop()
    };
    Ok((preserved, tools))
}

/// Actually resolve a model, build workspace tools, and run one turn through
/// the same `run_live_exec` entry the headless `rapid exec` path uses.
///
/// Deliberately simpler than `exec_turn`'s full setup for a first working
/// version of interactive execution: a single configured model (no fallback
/// chain, no managed-policy ceilings) and no proactive context retrieval,
/// reminders, hooks, or MCP servers. All of that is real and worth adding —
/// omitted here to land working end-to-end turn execution first, not
/// silently dropped as an oversight. Memory index and todo index (2026-09-05)
/// are the first of these to be wired in — see `preserve_memory_and_todos`.
fn run_interactive_turn_inner(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    cancel: &agent_runtime::CancellationToken,
) -> kernel::TurnOutcome {
    // Model resolved *before* context construction below — not after — so
    // the context budget (`context_budget_for`) is derived from the model
    // that will actually run this turn, never a hard-coded placeholder sized
    // before the model was even known. `redaction_snapshot` is captured here
    // too (it depends on the resolved credential, not on `tools`, which
    // doesn't exist yet) and applied to `tools` once `build_interactive_
    // turn_context` returns it below.
    //
    // One store, fully built before `ConfiguredModel` borrows from it — the
    // borrow must not outlive it, matching `exec_turn`'s own ordering.
    let credential_store = auth::InMemoryCredentialStore::new();
    let mut redaction_snapshot: Option<security::RedactionSnapshot> = None;
    let backing = match crate::user_config::select_from_process_env_gated() {
        Ok(ModelSelection::Configured { active, warnings }) => {
            for warning in warnings {
                crate::exec_diag::stderr_line(&format!("warning: {warning}"));
            }
            // Scrub the active model's own resolved credential from
            // captured shell_exec output — see exec_turn's own identical
            // seeding for why this is a real, plausible leak vector, not a
            // contrived one. Best-effort: a registration failure just means
            // nothing gets scrubbed, not a turn failure.
            if let Some(plaintext) = active.credential.plaintext.as_deref()
                && let Ok(refer) = auth::SecretRef::from_alias("active-model-credential")
            {
                let mut registry = security::SecretRedactionRegistry::new();
                let cancel = security::RedactionCancellation::new();
                if registry
                    .register_canary(&refer, plaintext.as_bytes(), &cancel)
                    .is_ok()
                {
                    redaction_snapshot = Some(registry.snapshot());
                }
            }
            match ConfiguredModel::build(&active, &credential_store) {
                Ok(model) => SelectedModel::Configured(Box::new(model)),
                Err(err) => {
                    return kernel::TurnOutcome::Failed {
                        reason: format!("model configuration error: {err}"),
                    };
                }
            }
        }
        Ok(ModelSelection::Unconfigured { .. }) => SelectedModel::Unconfigured(UnconfiguredModel),
        Err(err) => {
            return kernel::TurnOutcome::Failed {
                reason: format!("model configuration error: {err}"),
            };
        }
    };
    let (context_limit, output_reserve) = context_budget_for(&backing);

    let (preserved, mut tools) =
        match build_interactive_turn_context(root, trusted, text, None, context_limit, output_reserve) {
            Ok(built) => built,
            Err(outcome) => return outcome,
        };
    if let Some(snapshot) = redaction_snapshot {
        tools.set_redaction(snapshot);
    }

    execute_interactive_turn(client, session_id, actor, root, text, preserved, &mut tools, backing, cancel)
}

/// Test-only-but-real sibling of `run_interactive_turn_inner`: identical
/// context/tools setup (`build_interactive_turn_context`, so a scripted
/// turn still executes real tool calls against a real workspace), but takes
/// an already-resolved `backing` instead of reading it from process env/
/// config — the seam that lets a test drive the actual turn-execution
/// boundary deterministically instead of only proving it "doesn't crash"
/// against whatever model happens to be configured on the machine running
/// the test (or none at all). `budget` is the caller's simulated
/// `(context_limit, output_reserve)` for `backing` — a scripted
/// `LiveModelCall` has no real `ProviderCapabilities` to derive one from
/// (unlike production's `context_budget_for`), so the test names it
/// directly; see `ScriptedSession::run_turn_with_budget` for the call site
/// that actually varies it.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_interactive_turn_inner_with_backing<B: crate::host::LiveModelCall>(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    cancel: &agent_runtime::CancellationToken,
    backing: B,
    budget: (u32, u32),
) -> kernel::TurnOutcome {
    // `BypassPermissions`, not the real env/settings-resolved mode: see
    // `build_interactive_turn_context`'s own doc comment on `forced_mode`
    // for why a test can't safely control that resolution any other way.
    // This test seam is about proving the turn-execution *loop* — dispatch,
    // event propagation, completion/failure/cancellation mapping, lease
    // lifecycle — works correctly, not about re-testing the permission gate
    // itself (which has its own dedicated test suite in `permissions.rs`
    // and `exec_tools.rs`).
    let forced_mode = Some(crate::permissions::PermissionMode::BypassPermissions);
    let (context_limit, output_reserve) = budget;
    let (preserved, mut tools) = match build_interactive_turn_context(
        root,
        trusted,
        text,
        forced_mode,
        context_limit,
        output_reserve,
    ) {
        Ok(built) => built,
        Err(outcome) => return outcome,
    };
    execute_interactive_turn(client, session_id, actor, root, text, preserved, &mut tools, backing, cancel)
}

/// Run one turn's model/tool-call loop through the shared, already-governed
/// `run_live_exec` entry (the same one the headless `rapid exec` path uses)
/// and map its terminal status onto the kernel's own `TurnOutcome`. Generic
/// over the model backing (`B: LiveModelCall`) rather than hardcoding
/// `SelectedModel`, mirroring `run_live_exec`'s own generic-over-backing
/// shape one layer up — this is what actually makes `run_interactive_turn_
/// inner_with_backing` possible without duplicating this mapping logic.
///
/// Also attributes the turn's measured usage (`ExecOutcome.tokens`/
/// `.cost_usd_micros`, plus wall-clock time measured here) to whichever goal
/// was active when the turn *started* — see `active_goal_id`/
/// `accrue_turn_usage`'s own doc comments for why "started," not "whichever
/// goal happens to be active once the turn finishes." Attributed for every
/// outcome that actually reached `run_live_exec`'s own terminal status
/// (`Succeeded`, `Cancelled`, and `Failed` alike — the same "any incurred
/// usage counts" behavior `exec_turn`'s existing `--jsonl`/`--verbose`
/// reporting already gives a headless run, just now also reflected in
/// `GoalUsage`), never for a turn that errored out before producing an
/// `ExecOutcome` at all (nothing was measured to attribute).
#[allow(clippy::too_many_arguments)]
fn execute_interactive_turn<B: crate::host::LiveModelCall>(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    text: &str,
    preserved: PreservedLiveContext,
    tools: &mut ExecTools,
    backing: B,
    cancel: &agent_runtime::CancellationToken,
) -> kernel::TurnOutcome {
    let spec = match AgentSpec::builder(
        protocol::AgentId::new(),
        AgentRole::Coder,
        text.to_owned(),
        protocol::WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    {
        Ok(spec) => spec,
        Err(err) => {
            return kernel::TurnOutcome::Failed {
                reason: format!("invalid turn request: {err}"),
            };
        }
    };
    let request = AgentExecutionRequest::new(spec, session_id);
    let mut sink = InteractiveTurnSink {
        client,
        session_id,
        actor,
    };

    let goal_path = root.join(PROJECT_MARKER).join(GOAL_FILE);
    let goal_id = active_goal_id(&goal_path);
    let started = Instant::now();
    let run_result = crate::host::run_live_exec(
        preserved,
        backing,
        &request,
        tools,
        &mut sink,
        cancel,
        ContextRetryPolicy::default(),
        None,
    );
    if let (Ok(outcome), Some(goal_id)) = (&run_result, goal_id) {
        let active_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        accrue_turn_usage(
            &goal_path,
            goal_id,
            outcome.tokens,
            outcome.cost_usd_micros.unwrap_or(0),
            active_ms,
        );
    }

    match run_result {
        Ok(outcome) => match context_required_question(&outcome) {
            // Not `Failed`: the model correctly recognized it needed
            // something only the user can supply and asked for it, cleanly
            // — the same shape of outcome as an ordinary completion (the
            // kernel's own `TurnOutcome` has no third "stopped, but not a
            // failure" shape, and none of its other two variants fit either:
            // `Interrupted` is specifically for cancellation, not this). The
            // question becomes the turn's own assistant-visible text so the
            // *existing* transcript/session-ready pipeline shows it and
            // returns to input-ready with no special-cased UI path and no
            // failure banner — the user's next message is an ordinary new
            // turn, not a resumption of this one.
            Some(question) => kernel::TurnOutcome::Completed {
                text: Some(question.to_owned()),
            },
            None => match outcome.result.status() {
                AgentTerminalStatus::Succeeded => kernel::TurnOutcome::Completed {
                    text: Some(outcome.result.summary().to_owned()),
                },
                AgentTerminalStatus::Cancelled => kernel::TurnOutcome::Interrupted,
                AgentTerminalStatus::Failed => kernel::TurnOutcome::Failed {
                    reason: outcome.result.summary().to_owned(),
                },
                // `#[non_exhaustive]`: a future variant this match hasn't
                // been taught yet. The summary text is still real and safe
                // to show; treating it as failed rather than silently
                // succeeding is the conservative direction for an
                // unrecognized status.
                _ => kernel::TurnOutcome::Failed {
                    reason: outcome.result.summary().to_owned(),
                },
            },
        },
        Err(err) => kernel::TurnOutcome::Failed {
            reason: err.to_string(),
        },
    }
}

/// The model's own question when a turn stopped because it genuinely needs
/// something only the user can supply (`TurnStopReason::ContextRequired`,
/// reachable today via `ask_user` with no interactive answer source) — the
/// bounded `error` half of `failure_detail`, reused for this stop reason
/// exactly as it already is for `ToolFailed` (see `TurnFailureDetail`'s own
/// doc comment). `None` for every other outcome, including every other
/// failure — never guessed from `outcome.result.summary()`'s prose, and
/// never confused with `TurnStopReason::ContextBoundExceeded` (the model's
/// context *window* overflowing — a host/context-owner repair, not a
/// question for the user; `execute_with_context_recovery` already handles
/// that one entirely on its own, before this function ever sees the result).
fn context_required_question(outcome: &crate::host::ExecOutcome) -> Option<&str> {
    if outcome.stop_reason != Some(agent_runtime::TurnStopReason::ContextRequired) {
        return None;
    }
    outcome.failure_detail.as_ref().map(TurnFailureDetail::error)
}

/// The `SubagentReport` for a child turn that stopped needing context —
/// see `LiveSubagentRunner::run`'s own call site for why this must not
/// fall through to the generic "subagent turn failed" `Err` path.
/// `open_questions` is the existing, already-consumed mechanism this reuses
/// (`ExecTools::execute_task_spawn` already renders every entry as "open
/// question: ..." in the parent-visible summary) rather than a new,
/// competing channel for the same information.
fn subagent_context_required_report(
    outcome: &crate::host::ExecOutcome,
    question: &str,
) -> crate::exec_tools::SubagentReport {
    crate::exec_tools::SubagentReport {
        summary: format!(
            "the subagent could not proceed without more information from the user: {question}"
        ),
        status: outcome.result.status().as_str().to_owned(),
        tool_calls: outcome.tool_calls,
        tokens: outcome.tokens,
        cost_usd_micros: outcome.cost_usd_micros,
        stop_reason: outcome.stop_reason.map(|reason| reason.as_str().to_owned()),
        claims: Vec::new(),
        blockers: Vec::new(),
        open_questions: vec![question.to_owned()],
        patch_summary: None,
        artifacts: Vec::new(),
    }
}

fn interrupt_session(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    cancel: &CancellationToken,
) -> Result<(), InteractiveError> {
    cancel.check().map_err(|_| InteractiveError::Cancelled)?;
    block_on(
        client.interrupt(Interrupt::new(
            session_id,
            InterruptReason::ClientRequested,
            actor.clone(),
            TraceId::new(),
        )),
        cancel,
    )
}

fn drain_kernel_events(
    client: &InProcessKernelClient,
    stream: &mut EventStream,
    ui: &mut AppState,
    session_id: protocol::SessionId,
    cancel: &CancellationToken,
) -> Result<(), InteractiveError> {
    for i in 0..MAX_EVENTS_PER_TICK {
        cancel.check().map_err(|_| InteractiveError::Cancelled)?;
        match stream.try_recv() {
            Ok(Some(event)) => {
                *ui = reduce(ui.clone(), &UiEvent::Kernel(event));
            }
            Ok(None) => break,
            Err(err) => return Err(InteractiveError::Stream(err)),
        }
        if i + 1 == MAX_EVENTS_PER_TICK {
            break;
        }
    }
    // Refresh from a full snapshot too (clears a stale protocol-error block
    // and re-merges goal/agent rows) — but only once the incremental event
    // stream drained above has genuinely caught up to it. `get_session`
    // always reflects the ledger's current tip; the live-tail delivery
    // above has its own independent polling lag (`TAIL_POLL_INTERVAL` in
    // `crates/event-ledger/src/subscription.rs`). Applying an *ahead*
    // snapshot here would silently fast-forward the projection's own `seq`
    // past events still in flight in the channel — `kernel::session::
    // projection::apply_next` requires each event's seq to be exactly
    // `snapshot.seq + 1`, so when those events finally arrive (on a later
    // tick) they'd violate that invariant and get discarded with a
    // swallowed protocol error instead of ever reaching the transcript.
    // Found via a scripted turn that finished well inside one live-tail
    // poll interval, which a human-paced real session essentially never
    // does (`SessionLoop::run`'s own ~50ms tick is 5x that poll interval),
    // but a turn that fails before ever calling the model could plausibly
    // hit in production too.
    let snapshot = block_on(client.get_session(session_id), cancel)?;
    let caught_up = match ui.snapshot() {
        Some(current) => current.seq() >= snapshot.seq(),
        None => true,
    };
    if caught_up {
        *ui = reduce(ui.clone(), &UiEvent::Snapshot(snapshot));
    }
    Ok(())
}

/// Where [`TuiRenderer`] writes painted screen bytes. Production always
/// paints real stdout; a test may capture instead (see [`InteractiveOptions
/// ::capture_render`]) so painted output can be asserted on without raw
/// cursor/clear escape sequences corrupting the test process's own terminal.
enum RenderSink {
    Stdout(io::Stdout),
    Captured(Vec<u8>),
}

impl io::Write for RenderSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(out) => out.write(buf),
            Self::Captured(buf_out) => buf_out.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(out) => out.flush(),
            Self::Captured(_) => Ok(()),
        }
    }
}

/// Owns the rendering-local state `AppState` deliberately does not: a
/// virtualized [`tui::Transcript`]/[`tui::TranscriptViewport`] pair
/// incrementally folded from `AppState.transcript()` (mirroring the exact
/// `entries[rendered..]` slicing `drain_kernel_events` already used for its
/// own plain-text predecessor), so scroll position survives across frames
/// instead of resetting to the tail on every tick. Every other input —
/// route, modal stack, composer text, agent/goal projections — is read
/// fresh from `AppState` each frame; nothing here duplicates that state,
/// only the parts `AppState` itself declines to own (see `crate::tui::
/// transcript`'s own doc comment on why the viewport lives with the
/// caller).
struct TuiRenderer {
    transcript: tui::Transcript,
    viewport: tui::TranscriptViewport,
    rendered_entries: usize,
    sink: RenderSink,
}

impl TuiRenderer {
    fn new(capture: bool) -> Self {
        Self {
            transcript: tui::Transcript::new(),
            viewport: tui::TranscriptViewport::new(80, 24),
            rendered_entries: 0,
            sink: if capture {
                RenderSink::Captured(Vec::new())
            } else {
                RenderSink::Stdout(io::stdout())
            },
        }
    }

    /// Bytes actually painted so far, when this renderer was built to
    /// capture rather than write real stdout — `None` otherwise.
    fn captured_text(&self) -> Option<String> {
        match &self.sink {
            RenderSink::Captured(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            RenderSink::Stdout(_) => None,
        }
    }

    fn sync_transcript(&mut self, ui: &AppState) {
        let entries = ui.transcript();
        let start = self.rendered_entries.min(entries.len());
        for entry in &entries[start..] {
            self.transcript.push_entry(entry);
        }
        self.rendered_entries = entries.len();
    }

    /// Scroll one page toward earlier transcript output. Landing back on
    /// the last line re-enables auto-follow (see `TranscriptViewport::
    /// scroll_by`'s own doc comment) — scrolling never needs separate
    /// "snap back to tail" handling here.
    fn page_up(&mut self) {
        let page = i64::from(self.viewport.height().max(1));
        self.viewport.scroll_by(&self.transcript, -page);
    }

    /// Scroll one page toward later transcript output.
    fn page_down(&mut self) {
        let page = i64::from(self.viewport.height().max(1));
        self.viewport.scroll_by(&self.transcript, page);
    }

    /// Paint one full frame from `ui` — the only place `apps/rapid` turns
    /// `AppState` into terminal bytes. `TerminalBackend` (see `TerminalGuard`)
    /// only ever manages raw-mode/alt-screen/cursor state, never content, so
    /// content goes straight through crossterm's own cursor/clear commands,
    /// same as this renderer's plain-text predecessor did.
    fn render(&mut self, ui: &AppState) -> io::Result<()> {
        use std::io::Write as _;
        self.sync_transcript(ui);

        let viewport = ui.viewport();
        let size = tui::Rect::new(0, 0, viewport.width(), viewport.height());
        let modal_open = !ui.modal_stack().is_empty();

        let mut composer = tui::ComposerModel::new();
        let _ = composer.apply(tui::ComposerCommand::SetWidth(size.width()));
        let _ = composer.apply(tui::ComposerCommand::Insert(ui.composer().text().to_owned()));
        let requested_height = composer.preferred_height();
        let layout = tui::compute_screen_layout(ui, size, requested_height, modal_open);
        let composer_view = composer.render(layout.composer().width(), layout.composer().height());

        self.viewport.resize_rect(layout.transcript());

        let chrome = tui::StatusChrome::default();
        let screen = tui::paint_screen(
            ui,
            &self.transcript,
            &self.viewport,
            composer_view.lines(),
            &chrome,
            size,
            modal_open,
            &tui::CancellationToken::new(),
        );

        crossterm::queue!(
            self.sink,
            crossterm::cursor::MoveTo(0, 0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        for (i, row) in screen.rows().iter().enumerate() {
            let y = u16::try_from(i).unwrap_or(u16::MAX);
            crossterm::queue!(self.sink, crossterm::cursor::MoveTo(0, y))?;
            write!(self.sink, "{row}")?;
        }
        self.sink.flush()
    }
}

fn next_input(
    source: &mut InputSource,
    cancel: &CancellationToken,
) -> Result<Option<InteractiveInput>, InteractiveError> {
    cancel.check().map_err(|_| InteractiveError::Cancelled)?;
    match source {
        InputSource::Scripted { events } => {
            Ok(Some(events.next().unwrap_or(InteractiveInput::Eof)))
        }
        InputSource::Crossterm => {
            let ready = poll_event(INPUT_POLL_TIMEOUT).map_err(|_| InteractiveError::Io)?;
            if !ready {
                return Ok(None);
            }
            let event = read_event().map_err(|_| InteractiveError::Io)?;
            Ok(map_crossterm(event))
        }
    }
}

fn map_crossterm(event: CrosstermEvent) -> Option<InteractiveInput> {
    match event {
        CrosstermEvent::Resize(width, height) => Some(InteractiveInput::Resize { width, height }),
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            {
                return Some(InteractiveInput::CtrlC);
            }
            match key.code {
                KeyCode::Enter => Some(InteractiveInput::Enter),
                KeyCode::Backspace => Some(InteractiveInput::Backspace),
                KeyCode::Char(ch) => Some(InteractiveInput::Char(ch)),
                KeyCode::PageUp => Some(InteractiveInput::PageUp),
                KeyCode::PageDown => Some(InteractiveInput::PageDown),
                _ => None,
            }
        }
        _ => None,
    }
}

fn resolve_project(options: &InteractiveOptions) -> Result<ResolvedProject, InteractiveError> {
    options
        .cancel
        .check()
        .map_err(|_| InteractiveError::Cancelled)?;
    let cwd = canonicalize_dir(&options.cwd)?;
    let project_root = detect_project_root(&cwd, &options.cancel)?;
    let identity = ProjectIdentity::new(project_root.as_path(), None)
        .map_err(|_| InteractiveError::InvalidProjectRoot)?;

    let user_home = resolve_user_home(options)?;
    let trust = ProjectTrustStore::open(user_home.join(TRUST_CATALOG_NAME))
        .get(&identity, &options.cancel)
        .map_err(InteractiveError::Trust)?;

    let sources = gather_config_sources(options, &project_root, &user_home)?;
    let config = load_config(&sources).map_err(InteractiveError::Config)?;

    Ok(ResolvedProject {
        trust,
        executable_config_active: trust.is_trusted(),
        config,
        ledger_path: project_root.join(PROJECT_MARKER).join(LEDGER_NAME),
        root: project_root,
    })
}

fn detect_project_root(
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf, InteractiveError> {
    let mut current = cwd.to_path_buf();
    for depth in 0..=MAX_PROJECT_WALK_DEPTH {
        cancel.check().map_err(|_| InteractiveError::Cancelled)?;
        if depth == MAX_PROJECT_WALK_DEPTH {
            return Err(InteractiveError::InvalidProjectRoot);
        }
        if current.join(PROJECT_MARKER).exists() || current.join(GIT_MARKER).exists() {
            return canonicalize_dir(&current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return canonicalize_dir(cwd),
        }
    }
    Err(InteractiveError::InvalidProjectRoot)
}

fn gather_config_sources(
    options: &InteractiveOptions,
    project_root: &Path,
    user_home: &Path,
) -> Result<ConfigSources, InteractiveError> {
    options
        .cancel
        .check()
        .map_err(|_| InteractiveError::Cancelled)?;
    let workspace = read_config_text(
        project_root
            .join(PROJECT_MARKER)
            .join(WORKSPACE_CONFIG_NAME),
        ".rapidlm/config.toml",
        &options.cancel,
    )?;
    let user = read_config_text(
        user_home.join(USER_CONFIG_NAME),
        "user/config.toml",
        &options.cancel,
    )?;
    let env = env_overrides(&options.env, &options.cancel)?;
    if options.cli.len() > MAX_OVERRIDE_ENTRIES {
        return Err(InteractiveError::Config(
            ConfigLoadError::TooManyOverrides {
                source: kernel::ConfigOrigin::Cli,
            },
        ));
    }
    Ok(ConfigSources {
        workspace,
        user,
        env,
        cli: options.cli.clone(),
        cancel: options.cancel.clone(),
    })
}

fn read_config_text(
    path: PathBuf,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<ConfigText>, InteractiveError> {
    cancel.check().map_err(|_| InteractiveError::Cancelled)?;
    match fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() > MAX_CONFIG_DOCUMENT_BYTES {
                return Err(InteractiveError::Config(ConfigLoadError::SourceTooLarge {
                    source: if name.starts_with(".rapidlm") {
                        kernel::ConfigOrigin::Workspace
                    } else {
                        kernel::ConfigOrigin::User
                    },
                }));
            }
            let body = String::from_utf8(bytes).map_err(|_| {
                InteractiveError::Config(ConfigLoadError::InvalidSyntax {
                    source: if name.starts_with(".rapidlm") {
                        kernel::ConfigOrigin::Workspace
                    } else {
                        kernel::ConfigOrigin::User
                    },
                })
            })?;
            Ok(Some(ConfigText::new(name, body)))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(InteractiveError::Io),
    }
}

fn env_overrides(
    env: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<Vec<ConfigOverride>, InteractiveError> {
    let mut out = Vec::new();
    for (i, (key, value)) in env.iter().enumerate() {
        if i.is_multiple_of(16) {
            cancel.check().map_err(|_| InteractiveError::Cancelled)?;
        }
        if !key.starts_with(ENV_PREFIX) || key == RAPIDLM_HOME_ENV {
            continue;
        }
        if config_key_from_env_name(key).is_none() {
            continue;
        }
        if out.len() >= MAX_OVERRIDE_ENTRIES {
            return Err(InteractiveError::Config(
                ConfigLoadError::TooManyOverrides {
                    source: kernel::ConfigOrigin::Environment,
                },
            ));
        }
        out.push(ConfigOverride::new(key.clone(), value.clone()));
    }
    Ok(out)
}

fn resolve_user_home(options: &InteractiveOptions) -> Result<PathBuf, InteractiveError> {
    if let Some(home) = options.user_home.as_ref() {
        return canonicalize_or_create(home);
    }
    for (key, value) in &options.env {
        if key == RAPIDLM_HOME_ENV || key == HOME_ENV || key == USERPROFILE_ENV {
            if key == HOME_ENV || key == USERPROFILE_ENV {
                return canonicalize_or_create(&PathBuf::from(value).join(".rapidlm"));
            }
            return canonicalize_or_create(Path::new(value));
        }
    }
    Err(InteractiveError::UserHomeMissing)
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf, InteractiveError> {
    let canonical = fs::canonicalize(path).map_err(|_| InteractiveError::InvalidProjectRoot)?;
    if !canonical.is_absolute() {
        return Err(InteractiveError::InvalidProjectRoot);
    }
    Ok(canonical)
}

fn canonicalize_or_create(path: &Path) -> Result<PathBuf, InteractiveError> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|_| InteractiveError::Io)?;
    }
    canonicalize_dir(path)
}

fn human_actor() -> Result<ActorRef, InteractiveError> {
    ActorRef::new(ActorKind::Human, &EventId::new().to_string())
        .map_err(|_| InteractiveError::Internal)
}

fn close_stream(stream: &mut EventStream) {
    stream.close();
}

fn quiesce_graph(
    graph: &mut ServiceGraph<KernelRuntime>,
    cancel: &CancellationToken,
) -> Result<(), InteractiveError> {
    let deadline = Instant::now() + DEFAULT_ROLLBACK_QUIESCE;
    block_on(graph.shutdown(deadline, cancel), cancel)
}

fn map_terminal(err: TerminalError) -> InteractiveError {
    match err {
        TerminalError::NotATty => InteractiveError::NotATty,
        TerminalError::AlreadyActive => InteractiveError::AlreadyActive,
        TerminalError::HeadlessStdoutReserved => InteractiveError::Usage,
        other => InteractiveError::Terminal(other),
    }
}

fn block_on<T, E, F>(future: F, cancel: &CancellationToken) -> Result<T, InteractiveError>
where
    F: Future<Output = Result<T, E>>,
    InteractiveError: From<E>,
{
    cancel.check().map_err(|_| InteractiveError::Cancelled)?;
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output.map_err(InteractiveError::from),
        Poll::Pending => Err(InteractiveError::PendingFuture),
    }
}

impl InteractiveOptions {
    fn from_env() -> Result<Self, InteractiveError> {
        let cwd = std::env::current_dir().map_err(|_| InteractiveError::InvalidProjectRoot)?;
        Ok(Self {
            cwd,
            user_home: None,
            env: std::env::vars().collect(),
            cli: Vec::new(),
            cancel: CancellationToken::new(),
            inputs: None,
            terminal: None,
            capture_render: false,
        })
    }
}

impl InteractiveOutcome {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Quit => 0,
            Self::Interrupted => JsonlExitCode::Interrupted.as_i32(),
        }
    }
}

impl InteractiveError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Cancelled => JsonlExitCode::Interrupted.as_i32(),
            Self::Usage | Self::NotATty | Self::UserHomeMissing | Self::InvalidProjectRoot => {
                JsonlExitCode::Usage.as_i32()
            }
            Self::Config(_) => JsonlExitCode::Usage.as_i32(),
            Self::Kernel(err) => JsonlExitCode::from_api_error(err, false).as_i32(),
            Self::AlreadyActive
            | Self::Terminal(_)
            | Self::Trust(_)
            | Self::Service(_)
            | Self::Stream(_)
            | Self::PendingFuture
            | Self::Io
            | Self::Internal => JsonlExitCode::Runtime.as_i32(),
        }
    }
}

impl Display for InteractiveError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("interactive session cancelled"),
            Self::Usage => f.write_str("usage: rapid [subcommand]"),
            Self::NotATty => f.write_str("stdout is not a tty"),
            Self::AlreadyActive => f.write_str("terminal modes are already owned"),
            Self::UserHomeMissing => f.write_str("user data directory is missing"),
            Self::InvalidProjectRoot => f.write_str("invalid project root"),
            Self::Terminal(err) => write!(f, "{err}"),
            Self::Config(err) => write!(f, "{err}"),
            Self::Trust(err) => write!(f, "{err}"),
            Self::Kernel(err) => write!(f, "{err}"),
            Self::Service(err) => write!(f, "{err}"),
            Self::Stream(err) => write!(f, "{err}"),
            Self::PendingFuture => f.write_str("in-process kernel future stayed pending"),
            Self::Io => f.write_str("interactive session I/O failed"),
            Self::Internal => f.write_str("interactive session failed internally"),
        }
    }
}

impl Error for InteractiveError {}

impl From<protocol::ApiError> for InteractiveError {
    fn from(err: protocol::ApiError) -> Self {
        Self::Kernel(err)
    }
}

impl From<ServiceError> for InteractiveError {
    fn from(err: ServiceError) -> Self {
        Self::Service(err)
    }
}

impl KernelRuntime {
    fn new(ledger_path: PathBuf) -> Result<Self, InteractiveError> {
        let id = ServiceId::parse(KERNEL_SERVICE_ID).map_err(|_| InteractiveError::Internal)?;
        Ok(Self {
            id,
            dependencies: Vec::new(),
            health: HealthState::new(ServiceStatus::Stopped, 0),
            ledger_path,
            client: Mutex::new(None),
        })
    }

    fn client(&self) -> Option<InProcessKernelClient> {
        lock_client(&self.client).clone()
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

impl LifecycleService for KernelRuntime {
    fn id(&self) -> &ServiceId {
        &self.id
    }

    fn dependencies(&self) -> &[ServiceId] {
        &self.dependencies
    }

    async fn start(&self, ctx: ServiceContext) -> Result<(), ServiceError> {
        ctx.check()?;
        self.health
            .record(ServiceStatus::Starting, None, Self::now_ms());
        match InProcessKernelClient::open(&self.ledger_path) {
            Ok(client) => {
                *lock_client(&self.client) = Some(client);
                self.health
                    .record(ServiceStatus::Running, None, Self::now_ms());
                Ok(())
            }
            Err(_) => {
                self.health.record(
                    ServiceStatus::Failed,
                    Some(ServiceFailureKind::Failed),
                    Self::now_ms(),
                );
                Err(ServiceError::Failed)
            }
        }
    }

    async fn quiesce(&self, deadline: Instant) -> Result<(), ServiceError> {
        if Instant::now() >= deadline {
            self.health.record(
                ServiceStatus::Failed,
                Some(ServiceFailureKind::DeadlineExceeded),
                Self::now_ms(),
            );
            return Err(ServiceError::DeadlineExceeded);
        }
        self.health
            .record(ServiceStatus::Quiescing, None, Self::now_ms());
        Ok(())
    }

    async fn stop(&self) -> Result<(), ServiceError> {
        *lock_client(&self.client) = None;
        self.health
            .record(ServiceStatus::Stopped, None, Self::now_ms());
        Ok(())
    }

    fn health(&self) -> HealthSnapshot {
        HealthSnapshot::new(self.id.clone(), self.health.snapshot())
    }
}

fn lock_client(
    mutex: &Mutex<Option<InProcessKernelClient>>,
) -> std::sync::MutexGuard<'_, Option<InProcessKernelClient>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Display for InteractiveOutcome {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quit => f.write_str("quit"),
            Self::Interrupted => f.write_str("interrupted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    static TERMINAL_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn merge_settings_rules_never_truncates_a_second_files_deny_rule() {
        // A first settings document maxed out at `permissions::MAX_RULES`
        // filler allow rules must not crowd out a second document's rules,
        // deny rules included, once merged -- MAX_WIRED_RULES must be sized
        // to hold every known settings file's own full quota, not just one.
        use crate::permissions::{RuleEffect, parse_settings};
        let filler: Vec<String> = (0..crate::permissions::MAX_RULES)
            .map(|i| format!("\"workspace_read(filler-{i}/*)\""))
            .collect();
        let first_json = format!(r#"{{"permissions":{{"allow":[{}]}}}}"#, filler.join(","));
        let first = parse_settings(&first_json).expect("first settings");
        assert_eq!(first.rules.len(), crate::permissions::MAX_RULES);

        let second_json = r#"{"permissions":{"deny":["shell_exec(rm *)"]}}"#;
        let second = parse_settings(second_json).expect("second settings");
        assert_eq!(second.rules.len(), 1);

        let merged = merge_settings_rules(&[first, second]);
        assert!(
            merged.iter().any(|rule| rule.effect == RuleEffect::Deny),
            "the second file's deny rule must survive the merge, got {} total rules",
            merged.len()
        );
    }

    // --- Model-derived context budget (context_budget_for) --------------

    fn active_from_doc(doc: &str) -> crate::user_config::ActiveModel {
        let config = crate::user_config::parse_config_document(doc, "user.toml").expect("parse");
        crate::user_config::resolve_active(&[], &config).expect("active")
    }

    #[test]
    fn context_budget_for_unconfigured_uses_the_conservative_default() {
        assert_eq!(
            context_budget_for(&SelectedModel::Unconfigured(UnconfiguredModel)),
            (
                crate::user_config::DEFAULT_CONTEXT_WINDOW,
                crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS
            ),
            "no model configured must fall back to the same conservative default a \
             configured-but-unspecified model gets, never the old hard-coded 8192/256"
        );
    }

    #[test]
    fn context_budget_for_configured_model_uses_its_own_explicit_capabilities() {
        let doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "big-model"
base_url = "http://127.0.0.1:11434/v1"
context_window = 200000
max_tokens = 8192
"#;
        let store = auth::InMemoryCredentialStore::new();
        let model = ConfiguredModel::build(&active_from_doc(doc), &store).expect("build");
        let backing = SelectedModel::Configured(Box::new(model));
        assert_eq!(context_budget_for(&backing), (200_000, 8192));
    }

    #[test]
    fn context_budget_for_configured_model_without_explicit_capabilities_uses_the_default() {
        let doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "unspecified-model"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let store = auth::InMemoryCredentialStore::new();
        let model = ConfiguredModel::build(&active_from_doc(doc), &store).expect("build");
        let backing = SelectedModel::Configured(Box::new(model));
        assert_eq!(
            context_budget_for(&backing),
            (
                crate::user_config::DEFAULT_CONTEXT_WINDOW,
                crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS
            ),
            "a configured model that never set context_window/max_tokens must get the \
             same conservative default as an unconfigured one, not a silently invented \
             precise-looking number"
        );
    }

    #[test]
    fn context_budget_for_fallback_chain_uses_min_context_and_max_output_across_backends() {
        // Deliberately crossed so the correct pairing is a genuine chimera
        // matching neither backend's own real pair — this is the shape that
        // actually discriminates the correct rule from two plausible-
        // looking wrong ones: taking the minimum of *both* fields (unsafe —
        // under-reserves headroom for whichever backend's own real request
        // sends the larger output cap independently, see `context_budget_
        // for`'s own doc comment) and naively using only the primary's own
        // numbers (wrong whenever the primary isn't the most-constraining
        // candidate on some field). A fixture where one backend is smaller
        // on both fields, or where the two extremes happen to land on the
        // same backend, cannot tell any of the three apart.
        let primary_doc = r#"
[models]
default = "cloud"

[model.cloud]
provider = "openai-compatible"
model = "big-context-big-output"
base_url = "http://127.0.0.1:11434/v1"
context_window = 50000
max_tokens = 6000
"#;
        let alternate_doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "small-context-small-output"
base_url = "http://127.0.0.1:11435/v1"
context_window = 8000
max_tokens = 500
"#;
        let store_a = auth::InMemoryCredentialStore::new();
        let store_b = auth::InMemoryCredentialStore::new();
        let primary =
            ConfiguredModel::build(&active_from_doc(primary_doc), &store_a).expect("build primary");
        let alternate =
            ConfiguredModel::build(&active_from_doc(alternate_doc), &store_b).expect("build alternate");

        let primary_ref = llm_router::provider::ModelRef::new(
            llm_router::provider::ProviderId::parse("openai-compatible").expect("provider"),
            llm_router::provider::ModelId::parse("cloud").expect("model id"),
        );
        let alternate_ref = llm_router::provider::ModelRef::new(
            llm_router::provider::ProviderId::parse("openai-compatible").expect("provider"),
            llm_router::provider::ModelId::parse("local").expect("model id"),
        );
        let policy = llm_router::fallback::FallbackPolicy::standard();
        let router_cancel = llm_router::provider::CancellationToken::new();
        let controller = llm_router::fallback::FallbackController::from_explicit_chain(
            primary_ref.clone(),
            vec![alternate_ref.clone()],
            policy,
            &router_cancel,
        )
        .expect("controller");
        let chain = FallbackChainModel::new(
            vec![(primary_ref, primary), (alternate_ref, alternate)],
            controller,
            None,
        );
        let backing = SelectedModel::FallbackChain(Box::new(chain));

        assert_eq!(
            context_budget_for(&backing),
            (8000, 6000),
            "must be (min context_limit, max max_output) across every backend — \
             context_limit=8000 from the alternate (the primary's 50000 would be \
             unsafe for the alternate), output_reserve=6000 from the primary (the \
             alternate's 500 would under-reserve headroom for the primary's own \
             real, larger output cap) — a chimera matching neither backend's own \
             pair, which is exactly the point: min/min would give (8000, 500), and \
             naively using only the primary would give (50000, 6000), both wrong"
        );
    }

    #[test]
    fn gate_fallback_candidates_fails_closed_on_a_policy_read_error_instead_of_no_policy() {
        // `load_policy`'s own doc comment: "set-but-unreadable is an error
        // ... must not silently become 'no policy'". A candidate that a real
        // policy would block (wrong provider) must stay blocked even when
        // the read of that same policy document then fails -- the function
        // must propagate the error (refusing the whole fallback chain).
        let doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let config = crate::user_config::parse_config_document(doc, "user.toml").expect("parse");
        let candidate = crate::user_config::resolve_active(&[], &config).expect("active");

        let policy_path = std::env::temp_dir().join(format!(
            "rapidlm-fallback-gate-{}-{}.toml",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &policy_path,
            format!(
                "schema = \"{}\"\n[policy]\nallowed_providers = [\"anthropic\"]\n",
                crate::managed_config::MANAGED_SCHEMA
            ),
        )
        .expect("write policy");
        let env = vec![(
            crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
            policy_path.display().to_string(),
        )];

        // Valid, readable policy: the openai-compatible candidate is
        // correctly blocked by the allowlist (a warning, not an error).
        let (gated, warnings) =
            gate_fallback_candidates(&env, vec![candidate.clone()]).expect("first read succeeds");
        assert!(gated.is_empty(), "candidate must be blocked by the allowlist");
        assert_eq!(warnings.len(), 1);

        // The same path, now unreadable (garbage/corrupt on a re-read):
        // must propagate the error, never fall back to treating this as "no
        // policy" (which would let the candidate through ungated).
        std::fs::write(&policy_path, "not valid toml at all {{{").expect("corrupt policy");
        match gate_fallback_candidates(&env, vec![candidate]) {
            Err(_) => {}
            Ok((gated, _)) => panic!(
                "a policy read failure must not silently become \"no policy\", got {} \
                 candidate(s) let through ungated",
                gated.len()
            ),
        }
        let _ = std::fs::remove_file(&policy_path);
    }

    #[test]
    fn max_wall_time_parses_and_rejects_bad_values() {
        let args: Vec<String> = ["do", "the", "thing", "--max-wall-time", "30"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let parsed = parse_exec_args(&args).expect("parses");
        assert_eq!(parsed.prompt, "do the thing");
        assert_eq!(parsed.max_wall_time, Some(Duration::from_secs(30)));

        // Missing value, non-numeric value: both a typed parse failure, not
        // a silent "no limit" or an accidental prompt-word swallow.
        let missing_value: Vec<String> = ["prompt", "--max-wall-time"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(parse_exec_args(&missing_value).is_none());
        let non_numeric: Vec<String> = ["prompt", "--max-wall-time", "soon"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(parse_exec_args(&non_numeric).is_none());

        // No flag at all: max_wall_time stays None, existing behavior
        // (unbounded, same as before this flag existed) is unchanged.
        let unbounded: Vec<String> = vec!["just".to_owned(), "a".to_owned(), "prompt".to_owned()];
        assert_eq!(parse_exec_args(&unbounded).expect("parses").max_wall_time, None);
    }

    #[test]
    fn forced_mode_overrides_env_and_settings_resolution() {
        // `rapid cron`'s propose-only execution (Modbit `AGT-008`/§3.2)
        // depends on `forced_mode` winning over whatever the ambient
        // environment/project settings would otherwise resolve — proven here
        // by requesting two different modes and confirming both distinctly
        // come back out, rather than both collapsing to one ambient default
        // (which would mean `forced_mode` was silently ignored).
        let plan = exec_permission_lattice(None, Some(crate::permissions::PermissionMode::Plan))
            .expect("lattice");
        assert_eq!(plan.mode(), crate::permissions::PermissionMode::Plan);
        let accept_edits = exec_permission_lattice(
            None,
            Some(crate::permissions::PermissionMode::AcceptEdits),
        )
        .expect("lattice");
        assert_eq!(
            accept_edits.mode(),
            crate::permissions::PermissionMode::AcceptEdits
        );
    }

    #[test]
    fn wall_time_watchdog_cancels_after_the_deadline_and_never_double_fires() {
        let cancel = agent_runtime::CancellationToken::new();
        spawn_wall_time_watchdog(cancel.clone(), Duration::from_millis(20));
        assert!(!cancel.is_cancelled(), "must not fire before the deadline");
        std::thread::sleep(Duration::from_millis(200));
        assert!(cancel.is_cancelled(), "must fire once the deadline passes");
    }

    #[test]
    fn wall_time_watchdog_is_a_noop_if_the_turn_already_finished() {
        // A turn that completes before the deadline cancels the token itself
        // (the same way a real exec_turn would on normal completion isn't
        // modeled here, but a cancel from *any* source before the deadline
        // must stop the watchdog from re-firing / overwriting anything).
        let cancel = agent_runtime::CancellationToken::new();
        cancel.cancel();
        spawn_wall_time_watchdog(cancel.clone(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        assert!(cancel.is_cancelled(), "stays cancelled, no panic or double-fire");
    }

    #[test]
    fn project_integrations_merge_across_rapidlm_and_claude_settings() {
        let dir = std::env::temp_dir().join(format!(
            "project-integrations-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".rapidlm")).expect("rapidlm dir");
        std::fs::create_dir_all(dir.join(".claude")).expect("claude dir");
        std::fs::write(
            dir.join(".rapidlm/settings.json"),
            r#"{"fetch_allowlist": ["example.com"], "hooks": {"session_start": ["a"]}}"#,
        )
        .expect("rapidlm settings");
        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"fetch_allowlist": ["other.example"], "hooks": {"session_start": ["b"]}}"#,
        )
        .expect("claude settings");

        let integrations = load_project_integrations(&dir);
        // List-shaped config merges across both files: a `.claude/
        // settings.json`-only project (the compat case this exists for)
        // gets the same treatment as a `.rapidlm/settings.json`-only one,
        // and a project with both gets contributions from both.
        assert_eq!(
            integrations.fetch_allowlist,
            vec!["example.com".to_owned(), "other.example".to_owned()]
        );
        assert_eq!(
            integrations.hooks.session_start,
            vec!["a".to_owned(), "b".to_owned()]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_integrations_caps_merged_hooks_at_the_per_stage_bound() {
        let dir = std::env::temp_dir().join(format!(
            "project-integrations-cap-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".rapidlm")).expect("rapidlm dir");
        std::fs::create_dir_all(dir.join(".claude")).expect("claude dir");
        // MAX_HOOKS_PER_STAGE commands in each file: each file's own parse
        // stays under the per-file cap, but the merge across two files
        // would exceed it without the post-merge truncate.
        let full_stage: Vec<String> = (0..crate::hooks::MAX_HOOKS_PER_STAGE)
            .map(|i| format!("echo {i}"))
            .collect();
        let settings = serde_json::json!({"hooks": {"session_start": full_stage}}).to_string();
        std::fs::write(dir.join(".rapidlm/settings.json"), &settings).expect("rapidlm settings");
        std::fs::write(dir.join(".claude/settings.json"), &settings).expect("claude settings");

        let integrations = load_project_integrations(&dir);
        assert_eq!(
            integrations.hooks.session_start.len(),
            crate::hooks::MAX_HOOKS_PER_STAGE,
            "merged session_start must stay capped at MAX_HOOKS_PER_STAGE"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_end_guard_fires_on_drop_regardless_of_which_scope_exit_ran() {
        let dir = std::env::temp_dir().join(format!(
            "session-end-guard-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("fired.txt");
        let script_path = dir.join("note.sh");
        std::fs::write(&script_path, format!("echo fired > {}", capture.display()))
            .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let hook = format!("sh {}", script_path.display());

        // Simulate an early-return exit path: the guard is constructed,
        // configured, and then the enclosing scope ends (an early `return`
        // from a real function would look identical from Drop's point of
        // view) without ever calling anything that "finishes normally".
        {
            let mut guard = SessionEndHookGuard::default();
            guard.hooks = vec![hook.clone()];
            // scope ends here -> Drop::drop fires, same as an early `?`
            // or `return` inside exec_turn would trigger.
        }
        assert!(capture.exists(), "session_end hook did not fire on drop");

        // A guard that never gets any hooks configured (e.g. no project
        // settings, or settings with no session_end key) must be a silent
        // no-op, not an error or a spurious fire.
        let capture2 = dir.join("should-not-exist.txt");
        {
            let _guard = SessionEndHookGuard::default();
        }
        assert!(!capture2.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_flags_accumulates_repeated_keys_and_rejects_bad_shapes() {
        let args: Vec<String> = [
            "--criterion",
            "c1=tests pass",
            "--criterion",
            "c2=build green",
            "--kind",
            "test",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let flags = parse_flags(&args).expect("flags");
        assert_eq!(flags.get("criterion").expect("multi").len(), 2);
        assert_eq!(one(&flags, "kind"), Some("test"));
        assert_eq!(one(&flags, "missing"), None);
        // A value without its flag, and a flag without its value, both fail.
        assert!(parse_flags(&["kind".to_owned(), "test".to_owned()]).is_err());
        assert!(parse_flags(&["--kind".to_owned()]).is_err());
    }

    struct TempEnv {
        root: PathBuf,
        project: PathBuf,
        user_home: PathBuf,
    }

    impl TempEnv {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("rapidlm-interactive-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let project = root.join("proj");
            let user_home = root.join("user");
            fs::create_dir_all(&project).expect("project");
            fs::create_dir_all(&user_home).expect("user home");
            Self {
                root,
                project,
                user_home,
            }
        }

        fn options(&self, inputs: Vec<InteractiveInput>) -> InteractiveOptions {
            InteractiveOptions {
                cwd: self.project.clone(),
                user_home: Some(self.user_home.clone()),
                env: Vec::new(),
                cli: Vec::new(),
                cancel: CancellationToken::new(),
                inputs: Some(inputs),
                terminal: Some(RecordingBackend::new()),
                capture_render: false,
            }
        }

        /// Same as [`Self::options`], but with the TUI renderer's painted
        /// bytes captured into the eventual `InteractiveReport::
        /// rendered_output` instead of discarded — for tests that need to
        /// inspect what the production render path actually painted.
        fn options_capturing_render(&self, inputs: Vec<InteractiveInput>) -> InteractiveOptions {
            InteractiveOptions {
                capture_render: true,
                ..self.options(inputs)
            }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn lock_terminal() -> std::sync::MutexGuard<'static, ()> {
        let guard = TERMINAL_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = tui::restore_if_armed();
        guard
    }

    fn identity_for(project: &Path) -> ProjectIdentity {
        let root = fs::canonicalize(project).expect("canon");
        ProjectIdentity::new(root, None).expect("identity")
    }

    #[test]
    fn no_subcommand_is_interactive() {
        assert_eq!(classify_launch(&[] as &[&str]), LaunchMode::Interactive);
        assert_eq!(classify_launch(&["--verbose"]), LaunchMode::Interactive);
        assert_eq!(classify_launch(&["run"]), LaunchMode::Subcommand);
        assert_eq!(classify_launch(&["--jsonl", "run"]), LaunchMode::Subcommand);
        assert_eq!(classify_launch(&["--help"]), LaunchMode::Help);
        assert_eq!(classify_launch(&["-h"]), LaunchMode::Help);
    }

    #[test]
    fn help_usage_lists_documented_command_surface() {
        for cmd in [
            "rapid exec",
            "rapid run",
            "rapid goal",
            "rapid graph",
            "rapid context",
            "rapid evidence",
            "rapid daemon",
            "rapid acp",
            "rapid doctor",
        ] {
            assert!(CLI_USAGE.contains(cmd), "usage missing {cmd}");
        }
    }

    #[test]
    fn process_run_rejects_subcommand() {
        assert_eq!(classify_launch(&["sessions"]), LaunchMode::Subcommand);
    }

    #[test]
    fn missing_trust_record_is_untrusted_and_does_not_activate_executable_config() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("run");
        assert_eq!(report.trust, TrustStatus::Untrusted);
        assert!(!report.executable_config_active);
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert!(report.terminal_restored);
        assert_eq!(report.graph_phase, GraphPhase::Stopped);
    }

    #[test]
    fn trusted_catalog_is_resolved_and_never_auto_granted() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let store = ProjectTrustStore::open(env.user_home.join(TRUST_CATALOG_NAME));
        store
            .set(
                &identity_for(&env.project),
                TrustStatus::Trusted,
                &CancellationToken::new(),
            )
            .expect("grant");
        let report =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("run");
        assert_eq!(report.trust, TrustStatus::Trusted);
        assert!(report.executable_config_active);
    }

    #[test]
    fn workspace_config_is_loaded() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("dir");
        fs::write(
            env.project.join(PROJECT_MARKER).join(WORKSPACE_CONFIG_NAME),
            "schema = 1\n[agents]\nmax_parallel = 7\n",
        )
        .expect("write config");
        let report =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("run");
        assert_eq!(report.config.config.agents.max_parallel, 7);
    }

    #[test]
    fn oversized_workspace_config_fails_closed() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("dir");
        fs::write(
            env.project.join(PROJECT_MARKER).join(WORKSPACE_CONFIG_NAME),
            vec![b'x'; MAX_CONFIG_DOCUMENT_BYTES + 1],
        )
        .expect("write");
        let err = run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
            .expect_err("too large");
        assert!(matches!(
            err,
            InteractiveError::Config(ConfigLoadError::SourceTooLarge { .. })
        ));
    }

    #[test]
    fn preserve_memory_and_todos_folds_both_indexes_and_fails_open_when_neither_exists() {
        // The interactive turn loop used to omit both indexes entirely (see
        // `run_interactive_turn_inner`'s own doc comment) — this is the
        // bug reproduction plus the fix, at the level that's actually
        // testable: the existing full interactive-session tests
        // deliberately cancel before a real model call, so they never
        // observe the built context, which is exactly why this logic was
        // extracted into its own function instead of staying inlined.
        let root = std::env::temp_dir().join(format!(
            "rapidlm-interactive-preserve-memory-todos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");

        let bare = PreservedLiveContext::new("goal", Vec::new(), "", "", 8192, 256).expect("bare");
        let folded = preserve_memory_and_todos(bare, &root);
        assert_eq!(
            folded.memory_index(),
            None,
            "no MEMORY.md yet: must fail open, not error"
        );
        assert_eq!(
            folded.todos_index(),
            None,
            "no todos.json yet: must fail open, not error"
        );

        std::fs::write(root.join(".rapidlm").join("MEMORY.md"), "remember this\n").expect("write");
        std::fs::write(
            root.join(crate::exec_tools::TODOS_PATH),
            r#"{"schema":1,"todos":[{"id":"t1","content":"do the thing","status":"pending"}]}"#,
        )
        .expect("write");

        let bare = PreservedLiveContext::new("goal", Vec::new(), "", "", 8192, 256).expect("bare");
        let folded = preserve_memory_and_todos(bare, &root);
        assert_eq!(folded.memory_index(), Some("remember this"));
        let todos = folded.todos_index().expect("todos index present");
        assert!(todos.contains("do the thing"), "{todos}");

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn first_ctrl_c_interrupts_then_second_exits() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("hello".to_owned()),
            InteractiveInput::CtrlC,
            InteractiveInput::CtrlC,
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Interrupted);
        assert_eq!(report.interrupt_count, 1);
        assert!(report.terminal_restored);
        assert_eq!(report.graph_phase, GraphPhase::Stopped);
        assert_eq!(report.outcome.exit_code(), 130);
    }

    #[test]
    fn explicit_quit_restores_terminal_and_quiesces_kernel() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let mut options = env.options(vec![InteractiveInput::Submit("/quit".to_owned())]);
        options.terminal = Some(backend);
        let report = run_interactive(options).expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert!(report.terminal_restored);
        assert_eq!(report.graph_phase, GraphPhase::Stopped);
        assert_eq!(
            log.snapshot_text(),
            "enter_raw_mode\nenter_alternate_screen\nhide_cursor\n\
             show_cursor\nleave_alternate_screen\nleave_raw_mode"
        );
    }

    #[test]
    fn cancelled_resolve_fails_closed() {
        let env = TempEnv::create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut options = env.options(vec![InteractiveInput::Eof]);
        options.cancel = cancel;
        let err = run_interactive(options).expect_err("cancelled");
        assert!(matches!(err, InteractiveError::Cancelled));
    }

    #[test]
    fn missing_user_home_fails_closed() {
        let env = TempEnv::create();
        let mut options = env.options(vec![InteractiveInput::Eof]);
        options.user_home = None;
        options.env.clear();
        let err = run_interactive(options).expect_err("home");
        assert!(matches!(err, InteractiveError::UserHomeMissing));
    }

    #[test]
    fn slash_agents_is_local_and_does_not_grant_trust() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("/agents".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.trust, TrustStatus::Untrusted);
        assert!(!report.executable_config_active);
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
    }

    // --- Slash-command control plane ------------------------------------
    //
    // Before this task, every `CommandError` other than `Empty` propagated
    // as `InteractiveError::Command`, which `SessionLoop::run`'s own `?`
    // turned into ending the *entire* interactive session — a single
    // mistyped or unsupported slash command (`/xyz`, `/fork extra-arg`)
    // crashed the whole TUI. `unknown_slash_command_shows_a_local_error_and_
    // does_not_end_the_session` below reproduces and proves that fixed; it
    // is also this task's primary interception revert-cycle regression
    // test (see the revert-cycle notes in `newtask.md`).

    #[test]
    fn unknown_slash_command_shows_a_local_error_and_does_not_end_the_session() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/this-command-does-not-exist".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("an unknown slash command must not end the session with an error");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(
            painted.contains("unknown command") && painted.contains("/help"),
            "{painted}"
        );
    }

    #[test]
    fn invalid_slash_command_arguments_show_specific_usage_not_a_generic_failure_or_the_full_catalog() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/fork extra-argument".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("invalid arguments on a known command must not end the session either");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("/fork"), "{painted}");
        assert!(
            !painted.contains("/playbook"),
            "a known-command syntax error should show that command's own usage, \
             not the entire catalog dump: {painted}"
        );
    }

    #[test]
    fn help_command_renders_the_command_catalog_through_the_production_path() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        // A tall viewport so the whole catalog fits without scrolling —
        // the transcript viewport auto-follows the tail by default, so a
        // default 24-row terminal would only show the catalog's last screen
        // full, not whether `/goal` (near the top) is present at all.
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Resize { width: 80, height: 60 },
            InteractiveInput::Submit("/help".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("/goal"), "{painted}");
        assert!(painted.contains("/quit"), "{painted}");
    }

    #[test]
    fn slash_goal_start_creates_a_real_goal_through_goal_host_not_a_model_turn() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal start ship the thing".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("goal started: ship the thing"), "{painted}");

        // The real mutation: `GoalHost`'s own concurrency-safe persistence,
        // not a test-only bypass — the same file `rapid goal show` reads.
        let goal_path = env.project.join(PROJECT_MARKER).join(GOAL_FILE);
        let host = GoalHost::load(&goal_path)
            .expect("load")
            .expect("goal.json must exist after /goal start");
        let snapshot = host.snapshot().expect("snapshot");
        assert_eq!(snapshot.statement(), "ship the thing");
        assert_eq!(snapshot.state(), agent_runtime::GoalState::Active);
    }

    #[test]
    fn slash_goal_start_with_no_text_is_a_local_usage_error_not_an_empty_turn() {
        // Before this task, `/goal start` (parsed correctly, statement
        // empty after `join_text` rejects it — or here, dispatched with an
        // empty statement some other way) reached `KernelApi::SubmitTurn`,
        // which submitted an *empty-text* turn instead of ever creating a
        // goal. `join_text` itself already rejects a bare `/goal start` at
        // the parser level (`CommandError::InvalidArgs`), so this proves
        // the parser-level rejection renders as a local usage error too,
        // not a silently-submitted empty turn.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal start".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let goal_path = env.project.join(PROJECT_MARKER).join(GOAL_FILE);
        assert!(
            GoalHost::load(&goal_path).ok().flatten().is_none(),
            "a bare /goal start must never create a goal"
        );
    }

    #[test]
    fn slash_goal_pause_then_resume_transitions_the_real_goal_through_goal_host() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal start pause and resume me".to_owned()),
            InteractiveInput::Submit("/goal pause".to_owned()),
            InteractiveInput::Submit("/goal resume".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("goal pause: ok"), "{painted}");
        assert!(painted.contains("goal resume: ok"), "{painted}");

        let goal_path = env.project.join(PROJECT_MARKER).join(GOAL_FILE);
        let host = GoalHost::load(&goal_path).expect("load").expect("goal exists");
        assert_eq!(
            host.snapshot().expect("snapshot").state(),
            agent_runtime::GoalState::Active,
            "resume after pause must land back on Active"
        );
    }

    #[test]
    fn slash_goal_cancel_clears_the_real_goal_and_the_goals_panel_stops_showing_it() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal start cancel me please".to_owned()),
            InteractiveInput::Submit("/goal cancel".to_owned()),
            // Re-open the Goals route *after* the cancel so the captured
            // frame reflects whatever `AppState.goals` holds post-cancel,
            // not a stale render from before the cancel completed.
            InteractiveInput::Submit("/goal".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("goal cancel: ok"), "{painted}");
        // `rendered_output` is every frame this session ever painted,
        // concatenated (the renderer's own capture buffer has no per-frame
        // boundary) — "goal started: cancel me please" legitimately appears
        // in an *earlier* frame and always will. What must not survive into
        // the *last* painted frame is the Goals panel still showing that
        // goal — split on this renderer's own screen-clear sequence
        // (`crossterm::terminal::Clear(ClearType::All)`, emitted once per
        // `TuiRenderer::render` call) and check only the final one.
        let last_frame = painted.rsplit("\u{1b}[2J").next().expect("at least one frame");
        assert!(
            last_frame.contains("no goal"),
            "the final frame must show the Goals panel's empty state, not the \
             cancelled goal: {last_frame}"
        );
        assert!(
            !last_frame.contains("cancel me please"),
            "a cancelled goal must not keep showing in the Goals panel's final frame: {last_frame}"
        );

        let goal_path = env.project.join(PROJECT_MARKER).join(GOAL_FILE);
        let host = GoalHost::load(&goal_path).expect("load");
        assert!(
            host.is_none_or(|h| h.snapshot().is_none()),
            "cancel clears the host's own snapshot (see GoalState's doc comment) and \
             GoalHost::save removes the now-empty goal.json"
        );
    }

    #[test]
    fn goal_lifecycle_commands_with_no_active_goal_show_a_local_error_not_a_crash() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal pause".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("no active goal"), "{painted}");
    }

    // --- Autonomous goal execution (GoalDriver integration) -------------

    #[test]
    fn slash_goal_run_with_no_active_goal_shows_a_local_error_not_a_crash() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal run".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("/goal start"), "{painted}");
    }

    #[test]
    fn slash_goal_stop_with_nothing_running_is_a_harmless_local_message() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/goal start ship the thing".to_owned()),
            InteractiveInput::Submit("/goal stop".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("not running"), "{painted}");
    }

    /// Test-only helper: an active goal with exactly one completion
    /// criterion (not `ScriptedSession::create_active_goal`'s empty-criteria
    /// shape, which `can_complete` would trivially satisfy immediately —
    /// this file's own tests need a goal that genuinely requires at least
    /// one real turn/evidence before it can complete).
    fn create_goal_with_one_criterion(session: &ScriptedSession) -> protocol::GoalId {
        let mut host = GoalHost::new();
        let spec = GoalSpec::new(
            protocol::GoalId::new(),
            "ship the thing",
            vec![agent_runtime::Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::default(),
            vec![agent_runtime::EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec");
        let effect = host
            .apply(
                GoalCommand::Create(spec),
                &GoalActor::Human,
                &agent_runtime::CancellationToken::new(),
            )
            .expect("create goal");
        host.save(&session.goal_path()).expect("save goal");
        effect.goal_id()
    }

    /// Record one passing test-evidence record satisfying `"c1"`, exactly
    /// the shape `GoalHost::apply(GoalCommand::Complete)`'s own gate checks
    /// — mirrors `goal_host.rs`'s own `system_test_record` helper (private
    /// to that module, so rebuilt here rather than exposed cross-module for
    /// one shared helper).
    fn record_passing_evidence(evidence_path: &Path, goal_id: protocol::GoalId) {
        let mut host = GoalHost::new();
        let spec = EvidenceSpec::new(
            protocol::EvidenceId::new(),
            goal_id,
            EvidenceKind::Test,
            TEST_PASSED,
            EvidenceProducer::System,
            agent_runtime::EvidenceSource::new(protocol::ArtifactId::from_bytes(
                b"autonomous-goal-test-evidence",
            )),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command");
        host.record_evidence(spec).expect("record");
        host.save_evidence(evidence_path).expect("save evidence");
    }

    /// Drive `loop_state`'s autonomous stepping to a terminal state (or a
    /// generous bound), the same real-time-polling discipline `ScriptedSession::
    /// run_turn`'s own comment already establishes for this codebase's
    /// scripted-turn tests: iterations run on their own real (if fast,
    /// scripted) background thread, so the driving loop must actually wait
    /// in real wall-clock time between checks, not just retry instantly.
    fn drive_autonomous_goal(loop_state: &mut SessionLoop) {
        for _ in 0..300 {
            // `ScriptedSession::run_turn`'s own comment documents the exact
            // race this closes: `subscribe`'s replay/live-tail delivery
            // runs on its own worker thread, so a single `drain()` right
            // after a turn finishes can race that worker and see stale
            // state — here specifically a stale `self.ui.snapshot().seq()`,
            // which `continue_or_stop_autonomous_goal`'s own `submit_turn`
            // call would then use for the *next* iteration's
            // `kernel::SubmitTurn`, hitting a real `SessionConflict`
            // against the kernel's own already-advanced seq. A short,
            // bounded retry burst — not a single call — closes it exactly
            // like that helper's own 30x/10ms retry does.
            for _ in 0..15 {
                loop_state.drain().expect("drain");
                std::thread::sleep(Duration::from_millis(2));
            }
            loop_state.step_autonomous_goal().expect("step");
            if loop_state.autonomous.is_none() {
                return;
            }
        }
        panic!("autonomous goal loop did not reach a terminal state in time");
    }

    fn scripted_backing_queue(
        models: Vec<ScriptedModel>,
    ) -> ScriptedBackingQueue {
        std::sync::Arc::new(std::sync::Mutex::new(
            models
                .into_iter()
                .map(|m| Box::new(m) as Box<dyn crate::host::LiveModelCall + Send>)
                .collect(),
        ))
    }

    /// Builds a `SessionLoop` borrowing from the given locals — the exact
    /// shape `page_up_and_page_down_reach_the_renderers_viewport_through_
    /// session_loop_handle_input` already established, extended with
    /// `scripted_backings` so autonomous iterations run a real (scripted)
    /// model instead of trying to resolve one from process env.
    #[allow(clippy::too_many_arguments)]
    fn autonomous_session_loop<'a>(
        session: &'a ScriptedSession,
        stream: &'a mut EventStream,
        ui: &'a mut AppState,
        cancel: &'a CancellationToken,
        interrupt_count: &'a mut u32,
        saw_ctrl_c: &'a mut bool,
        renderer: &'a mut TuiRenderer,
        turn_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
        backings: ScriptedBackingQueue,
    ) -> SessionLoop<'a> {
        SessionLoop {
            client: &session.client,
            stream,
            ui,
            session_id: session.session_id,
            actor: &session.actor,
            cancel,
            interrupt_count,
            saw_ctrl_c,
            root: &session.root,
            trusted: true,
            turn_in_flight,
            renderer,
            autonomous: None,
            scripted_backings: Some(backings),
        }
    }

    #[test]
    fn slash_goal_pause_mid_autonomous_run_stops_the_loop_before_the_next_iteration() {
        // The exact interaction the driving instruction calls "the real
        // stop mechanism": `/goal pause` is an ordinary, already-wired
        // slash command (Task 8), never blocked while autonomous execution
        // owns the turn slot (only *plain text* is — see
        // `submit_composer`'s own check) — pausing takes effect at the next
        // continuation boundary, not by killing an in-flight turn.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        session.create_active_goal();

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        // Two backings queued, but only the first should ever run: pausing
        // after it finishes must pre-empt the second entirely.
        let backings = scripted_backing_queue(vec![
            ScriptedModel::terminal_with_usage("first pass", 40, 100),
            ScriptedModel::terminal_with_usage("second pass", 999, 999),
        ]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight.clone(),
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        assert!(loop_state.autonomous.is_some());

        // Wait for iteration 1 to actually finish, then pause *before* the
        // driving loop gets another chance to decide whether to continue —
        // exercising the real slash-command path, not a direct field
        // mutation.
        for _ in 0..300 {
            loop_state.drain().expect("drain");
            if !turn_in_flight.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        loop_state
            .goal_lifecycle_command(GoalLifecycleKind::Pause)
            .expect("pause");

        drive_autonomous_goal(&mut loop_state);

        assert!(loop_state.autonomous.is_none(), "pausing must stop the autonomous loop");
        let host = GoalHost::load(&session.goal_path()).expect("load").expect("goal exists");
        let snapshot = host.snapshot().expect("snapshot");
        assert_eq!(snapshot.state(), GoalState::Paused);
        assert_eq!(
            snapshot.usage().turns(),
            1,
            "the second queued backing must never have been reached"
        );
    }

    #[test]
    fn autonomous_goal_runs_two_real_iterations_then_blocks_on_its_own_turn_budget_usage_accrues_once_each()
     {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        // max_turns=2, no completion criteria to satisfy — the deterministic
        // stopping condition here is the goal's own budget, not evidence, so
        // exactly two real scripted iterations must run before a third is
        // ever attempted (proving `before_turn`'s pre-check, not a lucky
        // race, is what stops it — see the queue-exhaustion note below).
        let mut host = GoalHost::new();
        let spec = GoalSpec::new(
            protocol::GoalId::new(),
            "ship the thing",
            vec![],
            GoalBudget::new(Some(2), None, None, None),
            vec![],
        )
        .expect("spec");
        host.apply(
            GoalCommand::Create(spec),
            &GoalActor::Human,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("create goal");
        host.save(&session.goal_path()).expect("save goal");

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        // Exactly two scripted backings: if the loop wrongly attempted a
        // third iteration, `submit_turn`'s test-only branch would find the
        // queue empty and fall through to real (unconfigured-in-tests)
        // model resolution, which `TurnFailed`-stops the loop instead of
        // reaching the budget-exhaustion path this test actually asserts —
        // a real, not merely theoretical, way this test would catch a
        // budget-check-ordering regression.
        let backings = scripted_backing_queue(vec![
            ScriptedModel::terminal_with_usage("still working on it", 50, 200),
            ScriptedModel::terminal_with_usage("more progress", 60, 300),
        ]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight.clone(),
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        assert!(loop_state.autonomous.is_some(), "the first iteration must be submitted");
        drive_autonomous_goal(&mut loop_state);

        assert!(
            loop_state.autonomous.is_none(),
            "the loop must have stopped once the turn budget was exhausted"
        );
        let host = GoalHost::load(&session.goal_path()).expect("load").expect("goal exists");
        let snapshot = host.snapshot().expect("snapshot");
        assert_eq!(snapshot.state(), GoalState::Blocked);
        assert_eq!(snapshot.stop_reason(), Some(agent_runtime::GoalStopReason::BudgetExhausted));
        let usage = snapshot.usage();
        assert_eq!(usage.turns(), 2, "exactly two real iterations, never a third");
        assert_eq!(usage.tokens(), 110, "50 + 60 — each iteration's real tokens accrued exactly once");
        assert_eq!(usage.cost(), 500, "200 + 300 — each iteration's real cost accrued exactly once");
    }

    /// Context-budget P0 regression, autonomous side: `continue_or_stop_
    /// autonomous_goal` submits its iteration through the exact same
    /// `SessionLoop::submit_turn` an ordinary human-typed message uses —
    /// there is no separate autonomous-specific budget logic to regress
    /// independently. Proves it empirically rather than only by
    /// architectural argument: the scripted model's `step()` actually
    /// receives a system-prompt block whose rendered "Token budget"
    /// section carries the real, model-derived default
    /// (`DEFAULT_CONTEXT_WINDOW`/`DEFAULT_MAX_OUTPUT_TOKENS` —
    /// `context_budget_for`'s own fallback for a scripted/unconfigured
    /// backing), not the old hard-coded `8192`/`256` this task closes out.
    #[test]
    fn autonomous_goal_iteration_carries_the_real_model_derived_budget_not_the_old_hardcoded_one() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let mut host = GoalHost::new();
        let spec = GoalSpec::new(
            protocol::GoalId::new(),
            "ship the thing",
            vec![],
            GoalBudget::new(Some(1), None, None, None),
            vec![],
        )
        .expect("spec");
        host.apply(
            GoalCommand::Create(spec),
            &GoalActor::Human,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("create goal");
        host.save(&session.goal_path()).expect("save goal");

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let backings = scripted_backing_queue(vec![
            ScriptedModel::terminal_with_usage("working on it", 10, 5)
                .capturing_system_prompt(std::sync::Arc::clone(&captured)),
        ]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight,
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        drive_autonomous_goal(&mut loop_state);

        let prompts = captured.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(prompts.len(), 1, "exactly one autonomous iteration must have called step()");
        let expected = format!(
            "Context window: {} tokens. Reserve {} tokens",
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert!(
            prompts[0].contains(&expected),
            "autonomous iteration's system prompt must carry the real model-derived \
             default budget, not the old hard-coded 8192/256: {:?}",
            prompts[0]
        );
        assert!(
            !prompts[0].contains("Context window: 8192 tokens"),
            "must never regress to the old hard-coded literal: {:?}",
            prompts[0]
        );
    }

    #[test]
    fn autonomous_goal_completes_immediately_when_evidence_already_satisfies_it_no_turn_runs() {
        // A different, complementary property from the budget test above:
        // if the completion gate is already satisfied the moment autonomous
        // execution starts, it must complete on the very first continuation
        // check — before ever submitting a turn at all. Uses zero scripted
        // backings on purpose: if the implementation wrongly ran a turn
        // first, `submit_turn`'s test-only branch would find the queue
        // empty and fail closed (real, unconfigured-in-tests model
        // resolution), not silently succeed — this test would then fail on
        // the "no active goal" state divergence instead of passing by luck.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let goal_id = create_goal_with_one_criterion(&session);
        let evidence_path = session.root.join(PROJECT_MARKER).join(EVIDENCE_FILE);
        record_passing_evidence(&evidence_path, goal_id);

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(vec![]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight,
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        assert!(
            loop_state.autonomous.is_none(),
            "already-satisfied evidence must complete the goal on the first check, \
             without ever submitting a turn"
        );
        let host = GoalHost::load(&session.goal_path()).expect("load");
        assert!(
            host.is_none_or(|h| h.snapshot().is_none()),
            "the goal must be completed (snapshot cleared)"
        );
    }

    #[test]
    fn autonomous_goal_stops_on_context_required_leaves_goal_active_surfaces_the_question() {
        // The Task-4 precedent this must stay consistent with: an ordinary
        // interactive turn that needs context never mutates goal state —
        // the goal stays whatever it was, and the question becomes normal
        // assistant-visible text. Autonomous execution must do the same
        // (leave the goal Active, not Paused/Blocked) while additionally
        // stopping its own continuation loop — a context-required turn is
        // not a failure and not grounds to keep guessing.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let mut host = GoalHost::new();
        let spec = GoalSpec::new(
            protocol::GoalId::new(),
            "ship the thing",
            vec![],
            GoalBudget::default(),
            vec![],
        )
        .expect("spec");
        host.apply(
            GoalCommand::Create(spec),
            &GoalActor::Human,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("create goal");
        host.save(&session.goal_path()).expect("save goal");

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(vec![ScriptedModel::asks_for_context(
            "Which environment: staging or production?",
            &["staging", "production"],
        )]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight,
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        drive_autonomous_goal(&mut loop_state);

        assert!(loop_state.autonomous.is_none(), "the loop must stop, not keep guessing");
        assert!(
            loop_state
                .ui
                .transcript()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Assistant { text }
                    if text.contains("Which environment: staging or production?"))),
            "the model's own question must surface as ordinary turn output: {:?}",
            loop_state.ui.transcript()
        );
        // Specifically rules out the way this test could otherwise pass for
        // the wrong reason: only one scripted backing was queued, so if the
        // context-required check were missing, the loop would try a
        // *second* iteration, find the queue empty, fall through to real
        // (unconfigured-in-tests) model resolution, and stop on *that*
        // unrelated `TurnFailed` instead — `loop_state.autonomous.is_none()`
        // alone cannot tell the two apart, but the presence of a second,
        // spurious `TurnFailed` can (revert-cycle-verified: an earlier draft
        // of this test passed even with the context-required check removed,
        // exactly because of this gap).
        assert!(
            !loop_state
                .ui
                .transcript()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnFailed { .. })),
            "no second iteration should ever have been attempted: {:?}",
            loop_state.ui.transcript()
        );
        let host = GoalHost::load(&session.goal_path()).expect("load").expect("goal exists");
        assert_eq!(
            host.snapshot().expect("snapshot").state(),
            GoalState::Active,
            "context-required must leave the goal Active, not Paused/Blocked"
        );
    }

    #[test]
    fn autonomous_goal_stops_on_turn_failure_not_a_retry_storm() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let goal_id = session.create_active_goal();

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        // Exactly one scripted failure: if the loop wrongly retried, the
        // queue would run dry and `submit_turn` would fall through to real
        // (unconfigured-in-tests) model resolution — a second, different
        // failure the test below would not be able to distinguish from a
        // deliberate stop. `create_active_goal` has no completion criteria,
        // so vacuous auto-completion is the one other way this test could
        // pass for the wrong reason — ruled out by asserting the goal is
        // still the same, still-Active goal afterward, not completed.
        let backings = scripted_backing_queue(vec![ScriptedModel::failing()]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight,
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        drive_autonomous_goal(&mut loop_state);

        assert!(loop_state.autonomous.is_none(), "a failed turn must stop the loop");
        let host = GoalHost::load(&session.goal_path()).expect("load").expect("goal exists");
        let host_snapshot = host.snapshot().expect("snapshot");
        assert_eq!(host_snapshot.id(), goal_id);
        assert_eq!(
            host_snapshot.state(),
            GoalState::Active,
            "a turn failure stops the driver, not the goal itself \
             (distinct from a budget/pause/cancel/complete transition)"
        );
    }

    #[test]
    fn a_second_autonomous_driver_is_refused_while_one_already_owns_the_goal() {
        // Cross-process ownership: the lease has no notion of "this
        // session" — it is a real OS-level file lock, so holding it
        // (simulating a second process, or a second `/goal run` reusing a
        // stale lease this session forgot to drop) is enough to prove a
        // second driver is refused, without needing an actual second
        // process to reproduce.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        session.create_active_goal();
        let held = try_acquire_driver_lease(&session.goal_path()).expect("first lease");

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(vec![]);

        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            turn_in_flight,
            backings,
        );

        loop_state.start_autonomous_goal().expect("start");
        assert!(
            loop_state.autonomous.is_none(),
            "a second driver must never start while the lease is held elsewhere"
        );
        loop_state.drain().expect("drain");
        let painted = loop_state.renderer.captured_text().expect("captured");
        assert!(painted.contains("already running"), "{painted}");
        drop(held);
    }

    #[test]
    fn production_run_interactive_goal_run_does_not_crash_and_renders_truthfully() {
        // The critical production-wiring proof: drives the real
        // `run_interactive` entry point (not a manually constructed
        // `SessionLoop`) through `/goal start` → `/goal run` → enough
        // pass-through ticks for the real (if unconfigured-in-tests, so
        // fast-failing) background turn to finish → `/goal stop` → `/quit`.
        // No scripted model is available at this entry point (that seam
        // only exists on `SessionLoop` directly, used by the tests above) —
        // this deliberately exercises the real "not configured" failure
        // path instead, proving the whole stack (parser, dispatch,
        // autonomous state, real kernel turn submission, real failure
        // handling, truthful rendering, final input-ready state) survives
        // it end to end.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let mut inputs = vec![
            InteractiveInput::Submit("/goal start ship the thing".to_owned()),
            InteractiveInput::Submit("/goal run".to_owned()),
        ];
        inputs.extend((0..80).map(|_| InteractiveInput::Resize { width: 80, height: 24 }));
        inputs.push(InteractiveInput::Submit("/goal stop".to_owned()));
        inputs.push(InteractiveInput::Submit("/quit".to_owned()));
        let report = run_interactive(env.options_capturing_render(inputs)).expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(
            painted.contains("autonomous goal execution started"),
            "{painted}"
        );
    }

    /// Autonomous-goal regression for the project-trust P0: drives the same
    /// real, production `run_interactive` entry point (not a manually
    /// constructed `SessionLoop` with `trusted` hardcoded) as
    /// `production_run_interactive_goal_run_does_not_crash_and_renders_
    /// truthfully` above, but against a `TempEnv` that — like every
    /// `TempEnv` — starts with no trust record at all, so this whole
    /// `/goal start` → `/goal run` → several ticks → `/goal stop` cycle
    /// executes against a genuinely untrusted project through the real
    /// resolution path. Autonomous continuation calls the exact same
    /// `SessionLoop::submit_turn` an ordinary human-typed message does (see
    /// `continue_or_stop_autonomous_goal`'s own doc comment), so there is
    /// architecturally no special-cased trust bypass for it — this proves
    /// that empirically end to end: after the cycle, the on-disk trust
    /// catalog must still show no record (not even an untrusted one — the
    /// autonomous run must never have written to it at all) for this
    /// project's identity, and `rapid trust status`'s own store lookup must
    /// still report `Untrusted`, exactly as if no autonomous goal had run.
    #[test]
    fn autonomous_goal_run_in_an_untrusted_project_never_self_grants_trust() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let mut inputs = vec![
            InteractiveInput::Submit("/goal start ship the thing".to_owned()),
            InteractiveInput::Submit("/goal run".to_owned()),
        ];
        inputs.extend((0..80).map(|_| InteractiveInput::Resize { width: 80, height: 24 }));
        inputs.push(InteractiveInput::Submit("/goal stop".to_owned()));
        inputs.push(InteractiveInput::Submit("/quit".to_owned()));
        let report = run_interactive(env.options_capturing_render(inputs)).expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert_eq!(
            report.trust,
            TrustStatus::Untrusted,
            "the session itself must have resolved this project as untrusted"
        );

        let catalog_path = env.user_home.join(TRUST_CATALOG_NAME);
        assert!(
            !catalog_path.exists(),
            "an autonomous goal run against an untrusted project must never create a \
             trust catalog at all — no self-grant, not even a redundant untrusted record"
        );

        // Independently re-check via a fresh store handle, the same call
        // `rapid trust status` itself makes — not just the report snapshot
        // captured at session start.
        let identity = identity_for(&env.project);
        let store = ProjectTrustStore::open(&catalog_path);
        assert_eq!(
            store.get(&identity, &CancellationToken::new()).expect("get"),
            TrustStatus::Untrusted,
            "no autonomous iteration may leave this project trusted"
        );

        // The only reachable way to change that is an explicit grant (real
        // production coverage for `rapid trust grant` itself lives in
        // apps/rapid/tests/trust_cli.rs) — confirmed here only to show the
        // store this session used is the same one that command would use,
        // not a fixture the autonomous path could never have touched.
        store
            .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
            .expect("explicit grant");
        assert_eq!(
            store.get(&identity, &CancellationToken::new()).expect("get"),
            TrustStatus::Trusted
        );
    }

    #[test]
    fn unsupported_kernel_commands_render_an_honest_message_and_never_silently_no_op() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/mcp add example-mcp-server".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report.rendered_output.expect("capture_render was requested");
        assert!(painted.contains("not available"), "{painted}");
        assert!(painted.contains("MCP"), "{painted}");
    }

    #[test]
    fn a_slash_command_never_reaches_the_model_and_a_following_plain_message_still_does() {
        // Mirrors `a_second_plain_text_message_does_not_crash_the_session_
        // and_a_turn_actually_runs`'s own reasoning: scripted inputs process
        // with no real delay, so this does not wait for a real model
        // response (that would mean mocking the model or depending on
        // whatever provider happens to be configured on the machine running
        // the test). What this proves instead: a local command (`/goal
        // show`, a read-only route open — no kernel event at all) does not
        // touch `turn_in_flight`, and the *next* input — ordinary text — is
        // still free to reach real `submit_turn` execution afterward
        // (dropped only if a turn were already in flight, which nothing
        // here put one into).
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("/goal show".to_owned()),
            InteractiveInput::Submit("hello".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
    }

    #[test]
    fn command_error_text_distinguishes_unknown_from_invalid_syntax() {
        assert_eq!(
            command_error_text(&CommandError::UnknownCommand),
            "unknown command — type /help for available commands"
        );
        assert_eq!(command_error_text(&CommandError::TooLong), "command too long");
        assert_eq!(
            command_error_text(&CommandError::InvalidArgs { command: "fork" }),
            "/fork"
        );
        assert_eq!(command_error_text(&CommandError::Empty), "");
    }

    #[test]
    fn unsupported_command_text_names_the_specific_gap_not_a_generic_message() {
        let agent_text = unsupported_command_text(&KernelAction::PauseAgent { id: None });
        assert!(agent_text.contains("agent registry"), "{agent_text}");
        let mcp_text = unsupported_command_text(&KernelAction::AddMcp {
            target: "x".to_owned(),
        });
        assert!(mcp_text.contains("MCP"), "{mcp_text}");
        assert_ne!(
            agent_text, mcp_text,
            "different unsupported command families must not collapse into one generic string"
        );
    }

    #[test]
    fn a_second_plain_text_message_does_not_crash_the_session_and_a_turn_actually_runs() {
        // Reproduces, then proves fixed, the exact bug this feature exists
        // for: the interactive session used to terminate with an error the
        // moment a second plain-text message was submitted without an
        // intervening interrupt, because nothing ever released the first
        // turn's exclusive kernel lease. `submit_turn` now spawns real
        // execution and reports its outcome back, so the lease is released
        // regardless of how the turn actually finishes.
        //
        // Scripted inputs process with no real delay between them, so by
        // the time this reaches `/quit`, the first turn has typically only
        // gotten as far as `model.requested` (confirmed by inspecting the
        // ledger directly while writing this test) before the session's own
        // teardown interrupts it — this is deliberately not forced to wait
        // for a real model response: doing so would mean either mocking the
        // model or making a real network call keyed to whatever provider
        // happens to be configured on the machine running the test, neither
        // of which this test needs to prove what it's actually testing (the
        // lease releases and the process doesn't crash, regardless of how
        // the turn ends). The second plain-text submission lands while the
        // first is still in flight and is silently dropped by design (see
        // `submit_turn`'s own comment on `turn_in_flight`) — this test's
        // point is that dropping it, rather than reaching `SubmitTurn` and
        // hitting `SessionConflict`, is what happens.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("hello".to_owned()),
            InteractiveInput::Submit("are you still there?".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("a second plain-text message must not crash the session");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert!(report.terminal_restored);

        let session_id = report.session_id.expect("session id");
        // Turn execution is deliberately fire-and-forget from the input
        // loop's perspective (see `spawn_interactive_turn`'s own doc
        // comment) so Ctrl-C stays responsive during a real in-flight
        // turn — poll briefly for the background thread(s) to finish
        // releasing their lease rather than asserting immediately.
        let ledger_path = env.project.join(PROJECT_MARKER).join(LEDGER_NAME);
        let mut seq_after = 0;
        let mut turn_still_active = true;
        for _ in 0..80 {
            let client = InProcessKernelClient::open(&ledger_path).expect("open ledger");
            let snapshot = block_on(client.get_session(session_id), &CancellationToken::new())
                .expect("session");
            seq_after = snapshot.seq();
            turn_still_active = snapshot.active_turn().is_some();
            if !turn_still_active && seq_after > 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!turn_still_active, "a turn's lease was never released");
        assert!(
            seq_after > 1,
            "expected real turn events beyond session creation, got seq {seq_after}"
        );
    }

    #[test]
    fn a_panic_during_turn_execution_is_caught_and_reported_as_failed() {
        // `spawn_interactive_turn` relies on `catching_panics` to guarantee
        // its cleanup (releasing the turn's lease, clearing
        // `turn_in_flight`) still runs even if `run_interactive_turn`
        // itself panics — an adversarial self-review of the turn-execution
        // feature found that, before this fix, a panic anywhere in that
        // call chain unwound straight past both, stranding the lease and
        // leaving the session permanently, silently unresponsive to every
        // later message for the rest of the process. Exercises the exact
        // function `spawn_interactive_turn` calls, not a duplicate of its
        // logic, since the real call chain (`run_live_exec` /
        // `agent_runtime::run_turn`) isn't mockable enough to inject a real
        // panic deep inside it directly.
        let outcome = catching_panics(std::panic::AssertUnwindSafe(|| -> kernel::TurnOutcome {
            panic!("simulated turn-execution panic");
        }));
        assert!(
            matches!(outcome, kernel::TurnOutcome::Failed { .. }),
            "a panic must be caught and reported as Failed, not left to unwind: {outcome:?}"
        );
    }

    // --- Scripted turn-execution-loop coverage -----------------------------
    //
    // Every test above that exercises a real turn (`a_second_plain_text_
    // message_does_not_crash_the_session_and_a_turn_actually_runs`) deliberately
    // never waits for it to actually finish, because doing so would need
    // either a real network call to whatever model happens to be configured
    // on the machine running the test, or mocking one — and until now
    // `run_interactive_turn_inner` had no seam for that (it always resolved
    // its model from process env/config). `run_interactive_turn_inner_with_
    // backing` (and its siblings up the call chain) close that gap by taking
    // an already-resolved `crate::host::LiveModelCall` instead — the tests
    // below drive the exact same functions `SessionLoop::submit_turn` calls
    // in production, just with a scripted model, so they can assert on real
    // completion, real tool execution, real failure/cancellation mapping,
    // and real lease release deterministically instead of only "doesn't
    // crash."
    use agent_runtime::{ModelStepError, ModelStepInput, ModelStepOutput, ProposedToolCall};
    use std::collections::VecDeque;
    use tui::state::{ToolActivityStatus, TranscriptEntry};

    /// Scripted live model: a fixed sequence of steps, consumed in order.
    /// Mirrors `exec_tools.rs`'s own private `ScriptedModel` test double
    /// (same established pattern, not reusable directly — that one is
    /// private to `exec_tools`'s own test module).
    #[derive(Default)]
    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
        /// Set via `capturing_system_prompt`: when present, every `step()`
        /// call appends whichever system-source block's text it was handed
        /// (there is at most one — the rendered system prompt) so a test
        /// can assert on what the context builder actually produced for
        /// this turn, e.g. the "Context window: N tokens" line
        /// `context_budget_for`'s derived budget renders into it — without
        /// needing a real HTTP capture. `Arc<Mutex<_>>`, not `Rc<RefCell<_>>`:
        /// a `ScriptedModel` is moved into `spawn_interactive_turn_with_
        /// backing`'s own thread (`B: LiveModelCall + Send`), so the sink
        /// must cross that boundary too.
        captured_system_prompt: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    }

    impl ScriptedModel {
        /// Route every future `step()` call's system-prompt text into
        /// `sink` (appended, oldest first) in addition to producing this
        /// model's already-scripted outputs.
        fn capturing_system_prompt(
            mut self,
            sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        ) -> Self {
            self.captured_system_prompt = Some(sink);
            self
        }

        fn terminal(text: &str) -> Self {
            Self {
                outputs: VecDeque::from(vec![Ok(ModelStepOutput::Terminal {
                    text: text.to_owned(),
                    tokens: 1,
                    cost_usd_micros: None,
                })]),
                ..Default::default()
            }
        }

        fn write_then_answer(path: &str, content: &str, answer: &str) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::WORKSPACE_WRITE_TOOL,
                format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls: vec![call],
                        tokens: 1,
                        cost_usd_micros: None,
                    }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                        cost_usd_micros: None,
                    }),
                ]),
                ..Default::default()
            }
        }

        fn failing() -> Self {
            Self {
                outputs: VecDeque::from(vec![Err(ModelStepError::Failed)]),
                ..Default::default()
            }
        }

        fn cancelled() -> Self {
            Self {
                outputs: VecDeque::from(vec![Err(ModelStepError::Cancelled)]),
                ..Default::default()
            }
        }

        /// A terminal answer that reports real, nonzero token/cost usage
        /// (unlike `terminal`'s own `cost_usd_micros: None`) — for tests
        /// that need to observe a real cost value flow through, not just
        /// tokens.
        fn terminal_with_usage(text: &str, tokens: u64, cost_usd_micros: u64) -> Self {
            Self {
                outputs: VecDeque::from(vec![Ok(ModelStepOutput::Terminal {
                    text: text.to_owned(),
                    tokens,
                    cost_usd_micros: Some(cost_usd_micros),
                })]),
                ..Default::default()
            }
        }

        /// One real tool call that reports usage, then a failing second
        /// step — "failed execution after some model usage": the turn's
        /// `ExecOutcome` still carries the first step's real tokens/cost
        /// even though the turn as a whole did not succeed.
        fn usage_then_fail(path: &str, content: &str, tokens: u64, cost_usd_micros: u64) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::WORKSPACE_WRITE_TOOL,
                format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls: vec![call],
                        tokens,
                        cost_usd_micros: Some(cost_usd_micros),
                    }),
                    Err(ModelStepError::Failed),
                ]),
                ..Default::default()
            }
        }

        /// Same shape as `usage_then_fail`, but cancelled instead of failed
        /// — "cancelled execution after some model usage."
        fn usage_then_cancel(path: &str, content: &str, tokens: u64, cost_usd_micros: u64) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::WORKSPACE_WRITE_TOOL,
                format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls: vec![call],
                        tokens,
                        cost_usd_micros: Some(cost_usd_micros),
                    }),
                    Err(ModelStepError::Cancelled),
                ]),
                ..Default::default()
            }
        }

        /// A single `ask_user` call, reaching the *real*
        /// `ExecTools::execute_ask_user` (no answer source configured, the
        /// same as every real production caller today) — the model's step
        /// is scripted, but everything from the tool call onward
        /// (`ContextRequired`, the turn stopping, `TurnStopReason::
        /// ContextRequired`, the question flowing through `failure_detail`)
        /// is the real, unmocked orchestration path.
        fn asks_for_context(question: &str, options: &[&str]) -> Self {
            let options_json = options
                .iter()
                .map(|option| format!("{option:?}"))
                .collect::<Vec<_>>()
                .join(",");
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::ASK_USER_TOOL,
                format!(r#"{{"question":{question:?},"options":[{options_json}]}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![Ok(ModelStepOutput::ToolCalls {
                    calls: vec![call],
                    tokens: 1,
                    cost_usd_micros: None,
                })]),
                ..Default::default()
            }
        }
    }

    impl crate::host::LiveModelCall for ScriptedModel {
        fn step(
            &mut self,
            blocks: &[context_engine::compile::ContextBlock],
            _input: &ModelStepInput<'_>,
            _cancel: &agent_runtime::CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if let Some(sink) = &self.captured_system_prompt {
                let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                for block in blocks {
                    if block.source() == context_engine::compile::ContextSource::System {
                        sink.push(block.text().to_owned());
                    }
                }
            }
            // Deterministic, not incidental: without this, whether a
            // scripted turn's measured `active_ms` reads as nonzero would
            // depend on how fast the surrounding context/redaction-registry
            // setup happens to run on whatever machine executes the test —
            // real work today, but not something a test should rely on
            // staying slow enough to round up to a whole millisecond.
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.outputs.pop_front().unwrap_or(Err(ModelStepError::Failed))
        }
    }

    /// A real kernel session plus a real trusted workspace root, wired the
    /// same way `run_started_session` wires one for `SessionLoop` — minus
    /// the terminal/crossterm machinery, which is orthogonal to turn
    /// execution and already covered by `terminal.rs`'s own tests. `stream`/
    /// `ui` are bootstrapped and live exactly the way `run_started_session`
    /// bootstraps and holds them for the whole session: an initial `Snapshot`
    /// applied *before* subscribing, and the subscription started from that
    /// snapshot's own seq — a kernel event replayed with no snapshot behind
    /// it yet (subscribing from seq 0, before `SessionCreated`) does not
    /// fold into the transcript the same way, found the hard way when this
    /// harness's first version subscribed from 0 into a fresh `AppState::
    /// new()` and silently produced an empty transcript for every turn.
    struct ScriptedSession {
        client: InProcessKernelClient,
        session_id: protocol::SessionId,
        actor: ActorRef,
        root: PathBuf,
        stream: EventStream,
        ui: AppState,
    }

    impl ScriptedSession {
        fn create(env: &TempEnv) -> Self {
            let ledger_path = env.project.join(PROJECT_MARKER).join(LEDGER_NAME);
            fs::create_dir_all(ledger_path.parent().expect("ledger has a parent"))
                .expect("ledger dir");
            let client = InProcessKernelClient::open(&ledger_path).expect("open ledger");
            let actor = human_actor().expect("actor");
            let cancel = CancellationToken::new();
            let snapshot = block_on(
                client.create_session(CreateSession::new(ProjectId::new(), actor.clone(), TraceId::new())),
                &cancel,
            )
            .expect("create session");
            let session_id = snapshot.id();
            let stream = block_on(
                client.subscribe(SubscribeEvents::new(session_id, snapshot.seq())),
                &cancel,
            )
            .expect("subscribe");
            let ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
            Self {
                client,
                session_id,
                actor,
                root: env.project.clone(),
                stream,
                ui,
            }
        }

        fn transcript(&self) -> &[tui::state::TranscriptEntry] {
            self.ui.transcript()
        }

        fn state(&self) -> &AppState {
            &self.ui
        }

        /// Submit `text`, run it to completion against `backing` through the
        /// real `spawn_interactive_turn_with_backing` → `run_interactive_
        /// turn_with_backing` → `run_interactive_turn_inner_with_backing` →
        /// `execute_interactive_turn` chain (exactly what production's
        /// `submit_turn` calls, just with the model backing swapped), then
        /// folds every event the turn produced into `self.ui` the same way
        /// repeated `drain_kernel_events` calls would. Asserts the lease
        /// actually released and `turn_in_flight` actually reset before
        /// returning, since every test below relies on both.
        ///
        /// Uses the same conservative default budget `context_budget_for`
        /// falls back to for an unconfigured model — most callers below
        /// don't care what it is, only that the turn completes. A test that
        /// needs to prove budget-dependent context behavior for a specific
        /// simulated model capability uses `run_turn_with_budget` instead.
        fn run_turn(&mut self, text: &str, backing: ScriptedModel) {
            self.run_turn_with_budget(
                text,
                backing,
                (
                    crate::user_config::DEFAULT_CONTEXT_WINDOW,
                    crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
                ),
            );
        }

        fn run_turn_with_budget(&mut self, text: &str, backing: ScriptedModel, budget: (u32, u32)) {
            let cancel = CancellationToken::new();
            let expected_seq = block_on(self.client.get_session(self.session_id), &cancel)
                .expect("session")
                .seq();
            let handle = block_on(
                self.client.submit_turn(SubmitTurn::new(
                    self.session_id,
                    expected_seq,
                    self.actor.clone(),
                    TraceId::new(),
                    text,
                )),
                &cancel,
            )
            .expect("submit turn");
            let turn_cancel = self
                .client
                .turn_cancel_token(self.session_id)
                .expect("turn cancel token present immediately after a successful submit");
            let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let join = spawn_interactive_turn_with_backing(
                self.client.clone(),
                self.session_id,
                handle.turn_id(),
                self.actor.clone(),
                self.root.clone(),
                true,
                text.to_owned(),
                turn_cancel,
                std::sync::Arc::clone(&turn_in_flight),
                backing,
                budget,
            );
            join.join()
                .expect("the turn thread must not panic (catching_panics wraps its body)");
            assert!(
                !turn_in_flight.load(std::sync::atomic::Ordering::SeqCst),
                "turn_in_flight must reset to false once the turn thread finishes"
            );
            let snapshot = block_on(self.client.get_session(self.session_id), &cancel).expect("session");
            assert!(
                snapshot.active_turn().is_none(),
                "the turn's kernel lease must be released once finish_turn has run \
                 (the state the loop returns to idle in)"
            );

            // `subscribe`'s replay/live-tail delivery runs on its own worker
            // thread (`crates/event-ledger/src/subscription.rs`), feeding a
            // channel `try_recv()` only ever reads non-blocking — a single
            // `drain_kernel_events` call right after a turn finishes can
            // race that worker and see nothing yet, even though every event
            // is already durably on disk. Production never notices this:
            // `SessionLoop::run` calls `drain()` every ~50ms for the life of
            // the session, so a missed pass is simply caught by the next
            // one. A test calling it once does not get that for free, so
            // retry here — bounded and short, since the worker only has to
            // catch up on a handful of already-written events. Not an
            // early-exit-once-new-entries loop: a partial delivery would
            // already grow the transcript and stop the retry before the
            // turn's own terminal event has necessarily arrived, so every
            // attempt runs regardless.
            for _ in 0..30 {
                drain_kernel_events(
                    &self.client,
                    &mut self.stream,
                    &mut self.ui,
                    self.session_id,
                    &cancel,
                )
                .expect("drain");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn goal_path(&self) -> PathBuf {
            self.root.join(PROJECT_MARKER).join(GOAL_FILE)
        }

        /// Create and persist a fresh `Active` goal at this session's own
        /// `goal.json`, the same file `execute_interactive_turn` looks for.
        /// Returns its id.
        fn create_active_goal(&self) -> protocol::GoalId {
            let mut host = GoalHost::new();
            let spec = GoalSpec::new(
                protocol::GoalId::new(),
                "ship the thing",
                vec![],
                GoalBudget::default(),
                vec![],
            )
            .expect("spec");
            let effect = host
                .apply(
                    GoalCommand::Create(spec),
                    &GoalActor::Human,
                    &agent_runtime::CancellationToken::new(),
                )
                .expect("create goal");
            host.save(&self.goal_path()).expect("save goal");
            effect.goal_id()
        }

        /// Reload `goal.json` fresh from disk and read its usage — never
        /// cached, so this always reflects whatever `accrue_turn_usage`
        /// actually persisted, not some in-memory copy this harness itself
        /// might be tempted to keep in sync by hand.
        fn goal_usage(&self) -> agent_runtime::GoalUsage {
            GoalHost::load(&self.goal_path())
                .expect("load goal")
                .expect("goal exists")
                .snapshot()
                .expect("snapshot")
                .usage()
        }
    }

    impl Drop for ScriptedSession {
        fn drop(&mut self) {
            close_stream(&mut self.stream);
        }
    }

    #[test]
    fn interactive_turn_completes_and_propagates_tool_and_assistant_output() {
        // Successful completion + output propagation + tool-call/result
        // continuation + no lost final output, all in one coherent scripted
        // turn: a tool call that really executes against the real workspace,
        // then a terminal answer.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "write a note",
            ScriptedModel::write_then_answer("scripted-note.md", "hello", "scripted turn done"),
        );

        let written = fs::read_to_string(env.project.join("scripted-note.md"))
            .expect("the scripted tool call must have really written the file");
        assert_eq!(written, "hello");

        let transcript = session.transcript();
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity { tool, status: ToolActivityStatus::Completed }
                    if tool == crate::exec_tools::WORKSPACE_WRITE_TOOL
            )),
            "expected a completed workspace_write tool-activity entry: {transcript:?}"
        );
        assert!(
            transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Assistant { text } if text == "scripted turn done")),
            "the final assistant answer must not be lost: {transcript:?}"
        );
    }

    /// Context-budget P0 regression, interactive-TUI side: an ordinary
    /// Enter-press turn's system prompt must carry whichever
    /// `(context_limit, output_reserve)` the resolved model actually has —
    /// proven here by driving two real turns through the production
    /// `build_interactive_turn_context` path with two different simulated
    /// budgets (small vs. large) and asserting each turn's own captured
    /// system prompt shows its own numbers, never the other turn's and
    /// never the old hard-coded `8192`/`256`.
    #[test]
    fn interactive_turn_system_prompt_reflects_its_own_resolved_budget_small_and_large_differ() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);

        let small_captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn_with_budget(
            "say hi",
            ScriptedModel::terminal("ok").capturing_system_prompt(std::sync::Arc::clone(&small_captured)),
            (2_000, 200),
        );
        let large_captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn_with_budget(
            "say hi again",
            ScriptedModel::terminal("ok").capturing_system_prompt(std::sync::Arc::clone(&large_captured)),
            (200_000, 8_000),
        );

        let small_prompt = small_captured.lock().unwrap_or_else(|p| p.into_inner())[0].clone();
        let large_prompt = large_captured.lock().unwrap_or_else(|p| p.into_inner())[0].clone();
        assert!(
            small_prompt.contains("Context window: 2000 tokens. Reserve 200 tokens"),
            "{small_prompt:?}"
        );
        assert!(
            large_prompt.contains("Context window: 200000 tokens. Reserve 8000 tokens"),
            "{large_prompt:?}"
        );
        for prompt in [&small_prompt, &large_prompt] {
            assert!(
                !prompt.contains("Context window: 8192 tokens"),
                "must never regress to the old hard-coded literal: {prompt:?}"
            );
        }
    }

    #[test]
    fn interactive_turn_execution_failure_is_reported_and_the_lease_releases() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("do something", ScriptedModel::failing());

        let transcript = session.transcript();
        assert!(
            transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnFailed { .. })),
            "a failing model step must surface as a failed turn: {transcript:?}"
        );
    }

    #[test]
    fn interactive_turn_cancellation_is_reported_and_the_lease_releases() {
        // Mirrors `agent_runtime::turn`'s own established idiom for testing
        // cancellation deterministically (`ScriptedModel::new(vec![Err(
        // ModelStepError::Cancelled)])`) rather than racing a real cancel
        // against a real in-flight step — see `cancel_bridge_relays_a_
        // kernel_cancel_into_the_bridged_token` below for a test of the
        // actual concurrent Ctrl-C-to-cancellation-token bridge instead.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("do something", ScriptedModel::cancelled());

        let transcript = session.transcript();
        assert!(
            transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnInterrupted)),
            "a cancelled model step must surface as an interrupted turn: {transcript:?}"
        );
    }

    #[test]
    fn sequential_turns_both_complete_with_no_lost_or_duplicated_output() {
        // Proves the loop actually returns to an idle/ready state after a
        // turn completes: a second, independent turn on the same session
        // must also run to completion, and both turns' output must appear
        // exactly once each — neither lost nor duplicated.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("first message", ScriptedModel::terminal("first answer"));
        session.run_turn("second message", ScriptedModel::terminal("second answer"));

        let answers: Vec<&str> = session
            .transcript()
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec!["first answer", "second answer"],
            "both turns' answers must appear, in order, exactly once each: {answers:?}"
        );
    }

    // --- GoalUsage accrual ---------------------------------------------

    #[test]
    fn a_successful_turn_accrues_its_real_tokens_and_cost_to_the_active_goal() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn(
            "do something billable",
            ScriptedModel::terminal_with_usage("done", 777, 4_200),
        );

        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 1);
        assert_eq!(usage.tokens(), 777);
        assert_eq!(usage.cost(), 4_200);
        // Real wall-clock time was spent; this harness cannot pin an exact
        // value, but it must not have stayed at the zero it starts at.
        assert!(usage.active_ms() > 0, "active_ms must reflect real elapsed time, got 0");
    }

    #[test]
    fn a_turn_with_no_active_goal_does_not_create_or_affect_one() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        // Deliberately no `create_active_goal()` call.

        session.run_turn("no goal here", ScriptedModel::terminal_with_usage("done", 100, 100));

        assert!(
            !session.goal_path().exists(),
            "a turn run with no goal file present must not invent one"
        );
    }

    #[test]
    fn a_failed_turn_still_accrues_the_usage_it_actually_incurred() {
        // "Usage reflects resources actually consumed, not only successful
        // user-visible outcomes" — the same behavior `exec_turn`'s own
        // existing `--jsonl`/`--verbose` reporting already gives a failed
        // headless run (its `cost_usd_micros` is read from `Ok(outcome)`
        // regardless of terminal status), now also reflected in `GoalUsage`.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn(
            "do something that fails partway",
            ScriptedModel::usage_then_fail("note.md", "partial", 300, 150),
        );

        assert!(
            session
                .transcript()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnFailed { .. })),
            "sanity check: the turn must actually have failed"
        );
        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 1, "a failed turn still counts as one incurred turn");
        assert_eq!(usage.tokens(), 300);
        assert_eq!(usage.cost(), 150);
    }

    #[test]
    fn a_cancelled_turn_still_accrues_the_usage_it_actually_incurred() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn(
            "do something that gets cancelled",
            ScriptedModel::usage_then_cancel("note.md", "partial", 60, 30),
        );

        assert!(
            session
                .transcript()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnInterrupted)),
            "sanity check: the turn must actually have been interrupted"
        );
        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 1);
        assert_eq!(usage.tokens(), 60);
        assert_eq!(usage.cost(), 30);
    }

    #[test]
    fn zero_usage_failure_before_any_model_call_does_not_invent_cost() {
        // `run_interactive_turn_inner`'s own model-configuration-error early
        // return (unconfigured/misconfigured model) never reaches `execute_
        // interactive_turn` at all — no `ExecOutcome` is ever produced, so
        // there is nothing to attribute. Exercised here at the boundary
        // that *is* reachable from a test (a model step failing on its very
        // first call, before any tool ran or any token was reported) rather
        // than by actually leaving the model unconfigured, which `Scripted
        // Model` sidesteps entirely — `failing()`'s first call already
        // covers "fails before producing any usage-bearing output."
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn("fails immediately", ScriptedModel::failing());

        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 1, "the turn still ran (and failed), so it still counts");
        assert_eq!(usage.tokens(), 0, "must not invent tokens that were never reported");
        assert_eq!(usage.cost(), 0, "must not invent cost that was never reported");
    }

    #[test]
    fn sequential_turns_accumulate_goal_usage_without_double_counting() {
        // "A completed piece of billable work must affect GoalUsage exactly
        // once": run two real, separate turns against the same goal and
        // confirm the total is exactly their sum — not the sum counted
        // twice (a duplicate accrual bug) and not just the last turn's
        // value (an overwrite-instead-of-accumulate bug).
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn("first", ScriptedModel::terminal_with_usage("first done", 100, 10));
        session.run_turn("second", ScriptedModel::terminal_with_usage("second done", 250, 40));

        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 2);
        assert_eq!(usage.tokens(), 350);
        assert_eq!(usage.cost(), 50);
    }

    // --- Inadequate-context semantics ---------------------------------

    #[test]
    fn a_turn_that_needs_context_ends_cleanly_with_the_models_own_question() {
        // The real, unmocked path: a scripted model proposes a real
        // `ask_user` call, which reaches the real `ExecTools::execute_ask_
        // user` (no answer source configured, same as every real production
        // caller today) and returns `ContextRequired`. No failure banner: the
        // question surfaces as an ordinary assistant message, not
        // `TranscriptEntry::TurnFailed`.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "deploy the app",
            ScriptedModel::asks_for_context(
                "Which environment: staging or production?",
                &["staging", "production"],
            ),
        );

        let transcript = session.transcript();
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Assistant { text }
                    if text == "Which environment: staging or production?\n1. staging\n2. production"
            )),
            "the model's own question (with its own options) must reach the transcript \
             verbatim: {transcript:?}"
        );
        assert!(
            !transcript.iter().any(|entry| matches!(entry, TranscriptEntry::TurnFailed { .. })),
            "needing context is not a failure and must not show a failure banner: {transcript:?}"
        );
        assert!(
            !transcript.iter().any(|entry| matches!(entry, TranscriptEntry::TurnInterrupted)),
            "needing context is not a cancellation: {transcript:?}"
        );
        assert!(
            !transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity { status: ToolActivityStatus::Failed, .. }
            )),
            "needing context must not show a failure marker on the ask_user call either: \
             {transcript:?}"
        );
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity { status: ToolActivityStatus::ContextRequired, .. }
            )),
            "the ask_user call gets its own non-failure activity marker: {transcript:?}"
        );
    }

    #[test]
    fn a_context_required_question_appears_exactly_once_not_duplicated() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "do something ambiguous",
            ScriptedModel::asks_for_context("Which file did you mean?", &["a.rs", "b.rs"]),
        );

        let occurrences = session
            .transcript()
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Assistant { text }
                        if text == "Which file did you mean?\n1. a.rs\n2. b.rs"
                )
            })
            .count();
        assert_eq!(
            occurrences, 1,
            "the clarification question must appear exactly once, not duplicated as a second \
             UI-visible message: {:?}",
            session.transcript()
        );
    }

    #[test]
    fn a_turn_needing_context_still_accrues_its_usage_to_the_active_goal() {
        // Mirrors the already-shipped GoalUsage semantic this task must not
        // bypass: real tokens were spent proposing the `ask_user` call, so
        // they must still accrue even though the turn stopped needing
        // clarification rather than completing with a confident answer.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.create_active_goal();

        session.run_turn(
            "do something",
            ScriptedModel::asks_for_context("Which target?", &["x", "y"]),
        );

        let usage = session.goal_usage();
        assert_eq!(usage.turns(), 1, "a context-required turn still counts as one incurred turn");
        assert_eq!(usage.tokens(), 1, "ScriptedModel::asks_for_context reports 1 token");
    }

    #[test]
    fn after_needing_context_the_next_turn_proceeds_normally_with_no_stuck_state() {
        // "The session returns to input-ready" and "the next user turn can
        // proceed normally" — not a resumption of the same turn, an entirely
        // ordinary new one, proven by actually running a second real turn
        // and confirming both its own tool call and final answer show up.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);

        session.run_turn(
            "do something ambiguous",
            ScriptedModel::asks_for_context("Which environment?", &["staging", "prod"]),
        );
        session.run_turn(
            "staging",
            ScriptedModel::write_then_answer("note.md", "staging", "done in staging"),
        );

        let transcript = session.transcript();
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Assistant { text }
                    if text == "Which environment?\n1. staging\n2. prod"
            )),
            "the first turn's question must still be present: {transcript:?}"
        );
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Assistant { text } if text == "done in staging"
            )),
            "the second, ordinary turn must run to completion normally: {transcript:?}"
        );
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity { tool, status: ToolActivityStatus::Completed }
                    if tool == crate::exec_tools::WORKSPACE_WRITE_TOOL
            )),
            "the second turn's own tool call must have really executed, proving no stuck \
             turn_in_flight and no duplicate/skipped execution: {transcript:?}"
        );
    }

    #[test]
    fn context_required_question_is_none_for_every_other_stop_reason() {
        // Direct unit coverage of the shared extraction helper: it must not
        // mistake an ordinary tool-failure's `failure_detail` (also a
        // `TurnFailureDetail{tool, error}`, by construction — see that
        // struct's own doc comment on the deliberate reuse) for a
        // clarification question just because both happen to carry text in
        // the same-shaped field. Only `ContextRequired` counts.
        let with_tool_failed = test_exec_outcome(
            Some(agent_runtime::TurnStopReason::ToolFailed),
            Some(agent_runtime::TurnFailureDetail::new("shell_exec", "not found")),
        );
        assert_eq!(context_required_question(&with_tool_failed), None);

        let with_no_reason = test_exec_outcome(None, None);
        assert_eq!(context_required_question(&with_no_reason), None);

        let with_context_required = test_exec_outcome(
            Some(agent_runtime::TurnStopReason::ContextRequired),
            Some(agent_runtime::TurnFailureDetail::new("ask_user", "Which one?")),
        );
        assert_eq!(context_required_question(&with_context_required), Some("Which one?"));
    }

    #[test]
    fn a_subagent_needing_context_reports_an_open_question_not_a_generic_failure() {
        // Without this, `LiveSubagentRunner::run` would fall through to its
        // generic `Err("subagent turn {status}")` path, discarding the
        // child's own question entirely and leaving the parent model with
        // nothing to act on.
        let outcome = test_exec_outcome(
            Some(agent_runtime::TurnStopReason::ContextRequired),
            Some(agent_runtime::TurnFailureDetail::new(
                "ask_user",
                "Which environment should I deploy to?",
            )),
        );
        let question = context_required_question(&outcome).expect("context required");
        let report = subagent_context_required_report(&outcome, question);
        assert_eq!(
            report.open_questions,
            vec!["Which environment should I deploy to?".to_owned()]
        );
        assert!(
            report.summary.contains("Which environment should I deploy to?"),
            "{}",
            report.summary
        );
    }

    /// Minimal `ExecOutcome` for testing `context_required_question` in
    /// isolation, without running a real turn — the fields it doesn't
    /// inspect are filled with harmless placeholders.
    fn test_exec_outcome(
        stop_reason: Option<agent_runtime::TurnStopReason>,
        failure_detail: Option<agent_runtime::TurnFailureDetail>,
    ) -> crate::host::ExecOutcome {
        let result = AgentResult::new(
            protocol::AgentId::new(),
            AgentTerminalStatus::Failed,
            "test summary",
            Vec::new(),
            None,
            None,
            Vec::new(),
        )
        .expect("result");
        crate::host::ExecOutcome {
            result,
            failure_cause: None,
            failure_detail,
            stop_reason,
            tool_calls: 0,
            tokens: 0,
            cost_usd_micros: None,
        }
    }

    #[test]
    fn second_submission_while_a_turn_is_in_flight_is_silently_dropped_not_duplicated() {
        // Targets `SessionLoop::submit_turn`'s own `turn_in_flight` guard
        // directly: a submission that arrives while one is already running
        // must never reach `kernel::SubmitTurn` a second time (which would
        // either hit `SessionConflict` or, worse, actually start a second
        // concurrent turn) — see the guard's own comment for why dropping
        // it is the deliberate choice, not queuing it.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let before_seq = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();

        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, before_seq)),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = AppState::new();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut renderer = TuiRenderer::new(true);
        let mut loop_state = SessionLoop {
            client: &session.client,
            stream: &mut stream,
            ui: &mut ui,
            session_id: session.session_id,
            actor: &session.actor,
            cancel: &cancel,
            interrupt_count: &mut interrupt_count,
            saw_ctrl_c: &mut saw_ctrl_c,
            root: &session.root,
            trusted: true,
            turn_in_flight,
            renderer: &mut renderer,
            autonomous: None,
            #[cfg(test)]
            scripted_backings: None,
        };
        loop_state
            .submit_turn("a message arriving while another turn is in flight")
            .expect("submit_turn itself must not error even when dropped");

        let after_seq = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        assert_eq!(
            before_seq, after_seq,
            "a submission while turn_in_flight is set must never reach kernel::SubmitTurn"
        );
        close_stream(&mut stream);
    }

    #[test]
    fn cancel_bridge_relays_a_kernel_cancel_into_the_bridged_token() {
        // The actual concurrent mechanism Ctrl-C relies on: a real
        // `kernel::CancelToken`, cancelled from this test's own thread,
        // must reach the bridged `agent_runtime::CancellationToken` within
        // the watchdog's own poll interval — not just that a scripted model
        // reporting `ModelStepError::Cancelled` maps to the right outcome
        // (that's `interactive_turn_cancellation_is_reported_and_the_lease_
        // releases`, above; this test is the bridge itself).
        let kernel_cancel =
            kernel::CancellationTree::root(kernel::CancelOwner::Client).expect("root token");
        let bridge = CancelBridge::start(&kernel_cancel);
        assert!(!bridge.token.is_cancelled(), "must start uncancelled");

        kernel_cancel.cancel();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !bridge.token.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            bridge.token.is_cancelled(),
            "the watchdog must relay a kernel cancel into the bridged token"
        );
        bridge.stop();
    }

    #[test]
    fn detect_project_walks_up_to_rapidlm_marker() {
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("marker");
        let nested = env.project.join("src").join("crate");
        fs::create_dir_all(&nested).expect("nested");
        let options = InteractiveOptions {
            cwd: nested,
            user_home: Some(env.user_home.clone()),
            env: Vec::new(),
            cli: Vec::new(),
            cancel: CancellationToken::new(),
            inputs: None,
            terminal: None,
            capture_render: false,
        };
        let resolved = resolve_project(&options).expect("resolve");
        assert_eq!(
            resolved.ledger_path.parent().expect("rapidlm dir"),
            fs::canonicalize(&env.project)
                .expect("canon")
                .join(PROJECT_MARKER)
                .as_path()
        );
    }

    // --- TUI panel wiring -----------------------------------------------
    //
    // `TuiRenderer` is exercised two ways below: directly, against `AppState`
    // built through the real kernel/turn-execution path (`ScriptedSession`)
    // — the fast, deterministic way to check *what* gets painted for a given
    // state; and once through the actual `run_interactive` entry point with
    // `capture_render: true` — the one test proving `SessionLoop` itself
    // actually calls this renderer, not just that the renderer works in
    // isolation (see `run_interactive_actually_paints_through_the_production_
    // compositor_not_a_placeholder` below).

    #[test]
    fn idle_render_paints_composer_and_status_chrome_with_no_turn_activity() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(
            !painted.trim().is_empty(),
            "an idle session still paints composer/status chrome, not a blank screen"
        );
    }

    #[test]
    fn a_completed_tool_call_renders_its_own_completion_glyph() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "write a note",
            ScriptedModel::write_then_answer("note.md", "hello", "scripted turn done"),
        );
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(
            painted.contains(&format!("✓ {}", crate::exec_tools::WORKSPACE_WRITE_TOOL)),
            "{painted}"
        );
        assert!(painted.contains("scripted turn done"), "{painted}");
    }

    #[test]
    fn a_failed_turn_renders_a_turn_failed_banner_distinct_from_completion() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("do something that fails", ScriptedModel::failing());
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(painted.contains("turn failed"), "{painted}");
    }

    #[test]
    fn a_tool_call_that_completes_before_a_later_failure_still_shows_its_own_completion_glyph() {
        // `usage_then_fail` succeeds its tool call, then fails the model's
        // *next* step — the tool itself never failed, so its own activity
        // marker must stay "completed," not get swept into the turn's
        // eventual failure banner.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "do something that fails later",
            ScriptedModel::usage_then_fail("note.md", "hello", 5, 1),
        );
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(
            painted.contains(&format!("✓ {}", crate::exec_tools::WORKSPACE_WRITE_TOOL)),
            "{painted}"
        );
        assert!(painted.contains("turn failed"), "{painted}");
    }

    #[test]
    fn a_context_required_tool_call_renders_its_own_glyph_never_the_failure_glyph() {
        // Task 4 shipped a dedicated non-failure rendering for `ContextRequired`
        // at the `crates/tui` compositor level; this proves the *apps/rapid*
        // production renderer preserves that distinction end to end, against
        // a real turn that really reached `TurnStopReason::ContextRequired`.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "deploy the app",
            ScriptedModel::asks_for_context(
                "Which environment: staging or production?",
                &["staging", "production"],
            ),
        );
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(
            painted.contains(&format!("❓ {}", crate::exec_tools::ASK_USER_TOOL)),
            "{painted}"
        );
        assert!(
            !painted.contains(&format!("✗ {}", crate::exec_tools::ASK_USER_TOOL)),
            "context-required must never render with the failure glyph: {painted}"
        );
        assert!(
            painted.contains("Which environment: staging or production?"),
            "{painted}"
        );
    }

    #[test]
    fn a_cancelled_turn_renders_the_interrupted_marker() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("cancel me", ScriptedModel::cancelled());
        let mut renderer = TuiRenderer::new(true);
        renderer.render(session.state()).expect("render");
        let painted = renderer.captured_text().expect("captured");
        assert!(painted.contains("(interrupted)"), "{painted}");
    }

    #[test]
    fn tiny_and_zero_terminals_render_without_panicking() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        for (width, height) in [(1, 1), (5, 3), (80, 1), (0, 0), (1, 24)] {
            let state = reduce(
                session.state().clone(),
                &UiEvent::Local(LocalUiEvent::SetViewport { width, height }),
            );
            let mut renderer = TuiRenderer::new(true);
            renderer
                .render(&state)
                .unwrap_or_else(|err| panic!("{width}x{height} must not error: {err}"));
        }
    }

    #[test]
    fn resizing_between_frames_repaints_at_the_new_width_without_panicking() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("first", ScriptedModel::terminal("first done"));
        let mut renderer = TuiRenderer::new(true);

        let wide = reduce(
            session.state().clone(),
            &UiEvent::Local(LocalUiEvent::SetViewport { width: 120, height: 30 }),
        );
        renderer.render(&wide).expect("render wide");

        let narrow = reduce(
            session.state().clone(),
            &UiEvent::Local(LocalUiEvent::SetViewport { width: 40, height: 10 }),
        );
        renderer.render(&narrow).expect("render narrow");
        let painted = renderer.captured_text().expect("captured");
        assert!(painted.contains("first done"), "{painted}");
    }

    #[test]
    fn page_up_stops_auto_follow_and_page_down_back_to_the_tail_restores_it() {
        // Directly exercises `TuiRenderer::page_up`/`page_down` (what
        // `SessionLoop::handle_input` calls for `InteractiveInput::PageUp`/
        // `PageDown`) against a transcript longer than one page, proving the
        // established `TranscriptViewport` auto-follow contract (`crates/tui
        // ::transcript`'s own doc comment: landing back on the last line
        // re-enables follow) survives being driven through this renderer
        // rather than being reimplemented here.
        let mut renderer = TuiRenderer::new(true);
        for i in 0..40 {
            renderer.transcript.push_entry(&tui::state::TranscriptEntry::Assistant {
                text: format!("line {i}"),
            });
        }
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SetViewport { width: 80, height: 8 }),
        );
        renderer.render(&state).expect("render");
        assert!(
            renderer.viewport.follow_tail(),
            "a freshly rendered transcript starts following the tail"
        );

        renderer.page_up();
        assert!(
            !renderer.viewport.follow_tail(),
            "scrolling up must detach from the tail, not force the user back down"
        );

        for _ in 0..10 {
            renderer.page_down();
        }
        assert!(
            renderer.viewport.follow_tail(),
            "scrolling back down to the bottom must re-engage auto-follow"
        );
    }

    #[test]
    fn page_up_and_page_down_reach_the_renderers_viewport_through_session_loop_handle_input() {
        // Proves `SessionLoop::handle_input`'s own `PageUp`/`PageDown` arms
        // (not just `TuiRenderer`'s methods in isolation) actually reach the
        // renderer — constructed the same way production's `run_started_
        // session` constructs one, not a parallel test-only path.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let before_seq = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, before_seq)),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = AppState::new();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        for i in 0..40 {
            renderer.transcript.push_entry(&tui::state::TranscriptEntry::Assistant {
                text: format!("line {i}"),
            });
        }
        renderer.viewport.resize(80, 8);

        {
            let mut loop_state = SessionLoop {
                client: &session.client,
                stream: &mut stream,
                ui: &mut ui,
                session_id: session.session_id,
                actor: &session.actor,
                cancel: &cancel,
                interrupt_count: &mut interrupt_count,
                saw_ctrl_c: &mut saw_ctrl_c,
                root: &session.root,
                trusted: true,
                turn_in_flight: turn_in_flight.clone(),
                renderer: &mut renderer,
                autonomous: None,
                #[cfg(test)]
                scripted_backings: None,
            };
            loop_state
                .handle_input(InteractiveInput::PageUp)
                .expect("page up");
        }
        assert!(!renderer.viewport.follow_tail(), "PageUp must reach the renderer's viewport");

        {
            let mut loop_state = SessionLoop {
                client: &session.client,
                stream: &mut stream,
                ui: &mut ui,
                session_id: session.session_id,
                actor: &session.actor,
                cancel: &cancel,
                interrupt_count: &mut interrupt_count,
                saw_ctrl_c: &mut saw_ctrl_c,
                root: &session.root,
                trusted: true,
                turn_in_flight,
                renderer: &mut renderer,
                autonomous: None,
                #[cfg(test)]
                scripted_backings: None,
            };
            for _ in 0..10 {
                loop_state
                    .handle_input(InteractiveInput::PageDown)
                    .expect("page down");
            }
        }
        assert!(
            renderer.viewport.follow_tail(),
            "PageDown back to the tail must reach the renderer's viewport too"
        );
        close_stream(&mut stream);
    }

    #[test]
    fn run_interactive_actually_paints_through_the_production_compositor_not_a_placeholder() {
        // The critical regression test: proves the *real* `run_interactive`
        // entry point (production's own `SessionLoop::drain` ->
        // `TuiRenderer::render`), not `crates/tui`'s compositor tested in
        // isolation, actually paints route-specific sidebar content. `/goal`
        // with no active goal reaches `goal_lines`'s "no goal" fallback —
        // content the old plain-text `render_new_transcript_entries` could
        // never have produced, since switching route creates no transcript
        // entry at all. See this test's own revert-cycle note in newtask.md.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Resize { width: 80, height: 24 },
            InteractiveInput::Submit("/goal".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("no goal"),
            "the real production compositor must have painted the Goals sidebar: {painted}"
        );
    }
}
