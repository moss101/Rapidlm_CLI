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
use tui::state::{GoalLifecycle, GoalProjection, LocalUiEvent, MAX_COMPOSER_BYTES, UiEvent};
use tui::{
    AppState, CommandError, FrontendAction, FrontendKind, KernelAction, KernelApi, LocalAction,
    RecordingBackend, TerminalError, TerminalGuard, dispatch, parse_command, reduce,
};

use crate::goal_host::{EVIDENCE_FILE, GOAL_FILE, SESSIONS_DB_FILE, GoalHost};
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
    ContextRetryPolicy, EvidenceKind, EvidenceLedgerRef, EvidenceProducer, EvidenceSpec,
    EvidenceStatus, FailureCause, GoalActor, GoalBudget, GoalCommand, GoalSnapshot, GoalSpec,
    GoalState, TEST_PASSED, TurnFailureDetail, TurnStopReason,
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
    Command(CommandError),
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

Workspace tools stay disabled until the project is trusted: run `rapid`
interactively once in the project to approve trust.
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
            host.apply(command, &GoalActor::Human, &cancel)
                .map_err(|_| InteractiveError::Internal)?;
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
        "pause" => goal_lifecycle(&mut host, "pause", &cancel),
        "resume" => goal_lifecycle(&mut host, "resume", &cancel),
        "cancel" => goal_lifecycle(&mut host, "cancel", &cancel),
        "complete" => goal_lifecycle(&mut host, "complete", &cancel),
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
            let outcome = crate::goal_claim::run_claim(&mut host, ledger, claim, &cancel)
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
                "record" => goal_evidence_record(&mut host, &args[2..]),
                "list" => goal_evidence_list(&host),
                _ => Err(InteractiveError::Usage),
            }
        }
        _ => Err(InteractiveError::Usage),
    }?;

    if let Err(err) = host.save(&path) {
        eprintln!("{err}");
        return Ok(JsonlExitCode::Runtime.as_i32());
    }
    if let Err(err) = host.save_evidence(&evidence_path) {
        eprintln!("{err}");
        return Ok(JsonlExitCode::Runtime.as_i32());
    }
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

fn goal_evidence_record(host: &mut GoalHost, args: &[String]) -> Result<i32, InteractiveError> {
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
    let record = host.record_evidence(spec).map_err(|err| {
        eprintln!("{err}");
        InteractiveError::Internal
    })?;
    println!(
        "recorded {} kind={} status={} producer={}",
        record.id(),
        record.kind(),
        record.status(),
        record.producer()
    );
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
        // running with neither.
        let preserved = build_live_context(
            Some(&self.root),
            Some(&self.root),
            prompt.to_owned(),
            true,
            8192,
            256,
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

    // Prompt/context stack: project instructions (AGENTS.md convention +
    // compat paths) and the conditional-section system prompt (environment,
    // trust posture, token budget).
    let cwd = std::env::current_dir().ok();
    let preserved = build_live_context(
        workspace.as_ref().map(|(root, _)| root.as_path()),
        cwd.as_deref(),
        prompt.clone(),
        trusted,
        8192,
        256,
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
    // Reminder feeds: load the project roster if present and admit the
    // always-on feeds (the CLI host grants no capabilities, so feeds gated
    // on a capability stay inactive). A broken roster warns and the turn
    // continues without reminders — advisory context, kept not loaded.
    let mut reminder_floor = agent_runtime::reminders::ReminderFloor::Baseline;
    let preserved = match load_active_reminders() {
        Ok(Some((block, floor))) => {
            reminder_floor = floor;
            preserved.with_reminders_block(Some(block))
        }
        Ok(None) => preserved,
        Err(err) => {
            eprintln!("warning: reminders not loaded: {err}");
            preserved
        }
    };
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
approve trust by running `rapid` interactively once in this project, and set \
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

    // Layered model selection (env overrides > user config > typed fallback).
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
    }
    .run(&mut inputs);

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
}

impl SessionLoop<'_> {
    fn run(mut self, inputs: &mut InputSource) -> Result<InteractiveOutcome, InteractiveError> {
        loop {
            self.cancel
                .check()
                .map_err(|_| InteractiveError::Cancelled)?;
            self.drain()?;
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
        let text = trimmed.to_owned();
        self.submit_turn(&text)?;
        Ok(LoopControl::Continue)
    }

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
                FrontendAction::InlineHelp(_) => Ok(LoopControl::Continue),
                FrontendAction::Kernel(action) => {
                    self.apply_kernel_action(action)?;
                    Ok(LoopControl::Continue)
                }
            },
            Err(CommandError::Empty) => Ok(LoopControl::Continue),
            Err(err) => Err(InteractiveError::Command(err)),
        }
    }

    fn apply_kernel_action(&mut self, action: KernelAction) -> Result<(), InteractiveError> {
        self.cancel
            .check()
            .map_err(|_| InteractiveError::Cancelled)?;
        match action.kernel_api() {
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
                let to_seq = match &action {
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
            KernelApi::Approve | KernelApi::Dispatch => {}
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
        )
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

fn run_interactive_turn(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    kernel_cancel: &kernel::CancelToken,
) -> kernel::TurnOutcome {
    // `kernel::CancelToken` (set by `Interrupt`/Ctrl-C) and `agent_runtime::
    // CancellationToken` (what `run_live_exec` actually checks) are
    // different types from different crates with no dependency between
    // them — bridged with a poller, the same pattern already used for
    // `execute_mcp_tool`/`fetch_page`'s cross-crate cancellation, rather
    // than substituting a fresh, never-cancelled token that would make
    // Ctrl-C during a real in-flight turn silently do nothing.
    let bridge = agent_runtime::CancellationToken::new();
    let stop_watchdog = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let bridge = bridge.clone();
        let kernel_cancel = kernel_cancel.clone();
        let stop = std::sync::Arc::clone(&stop_watchdog);
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

    let outcome =
        run_interactive_turn_inner(client, session_id, actor, root, trusted, text, &bridge);

    stop_watchdog.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();
    outcome
}

/// Actually resolve a model, build workspace tools, and run one turn through
/// the same `run_live_exec` entry the headless `rapid exec` path uses.
///
/// Deliberately simpler than `exec_turn`'s full setup for a first working
/// version of interactive execution: a single configured model (no fallback
/// chain, no managed-policy ceilings) and no proactive context retrieval,
/// memory index, reminders, hooks, or MCP servers. All of that is real and
/// worth adding — omitted here to land working end-to-end turn execution
/// first, not silently dropped as an oversight.
fn run_interactive_turn_inner(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    cancel: &agent_runtime::CancellationToken,
) -> kernel::TurnOutcome {
    let preserved =
        match build_live_context(Some(root), Some(root), text.to_owned(), trusted, 8192, 256) {
            Ok(preserved) => preserved,
            Err(err) => {
                return kernel::TurnOutcome::Failed {
                    reason: format!("context error: {err}"),
                };
            }
        };
    let permission_lattice = match exec_permission_lattice(Some(root), None) {
        Ok(lattice) => lattice,
        Err(err) => {
            return kernel::TurnOutcome::Failed {
                reason: format!("permission configuration error: {err}"),
            };
        }
    };
    let mut tools = if trusted {
        ExecTools::workspace_with_permissions(root, permission_lattice)
            .unwrap_or_else(|_| ExecTools::noop())
    } else {
        ExecTools::noop()
    };

    // One store, fully built before `ConfiguredModel` borrows from it — the
    // borrow must not outlive it, matching `exec_turn`'s own ordering.
    let credential_store = auth::InMemoryCredentialStore::new();
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
                    tools.set_redaction(registry.snapshot());
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

    match crate::host::run_live_exec(
        preserved,
        backing,
        &request,
        &mut tools,
        &mut sink,
        cancel,
        ContextRetryPolicy::default(),
        None,
    ) {
        Ok(outcome) => match outcome.result.status() {
            AgentTerminalStatus::Succeeded => kernel::TurnOutcome::Completed {
                text: Some(outcome.result.summary().to_owned()),
            },
            AgentTerminalStatus::Cancelled => kernel::TurnOutcome::Interrupted,
            AgentTerminalStatus::Failed => kernel::TurnOutcome::Failed {
                reason: outcome.result.summary().to_owned(),
            },
            // `#[non_exhaustive]`: a future variant this match hasn't been
            // taught yet. The summary text is still real and safe to show;
            // treating it as failed rather than silently succeeding is the
            // conservative direction for an unrecognized status.
            _ => kernel::TurnOutcome::Failed {
                reason: outcome.result.summary().to_owned(),
            },
        },
        Err(err) => kernel::TurnOutcome::Failed {
            reason: err.to_string(),
        },
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
    let rendered = ui.transcript().len();
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
    let snapshot = block_on(client.get_session(session_id), cancel)?;
    *ui = reduce(ui.clone(), &UiEvent::Snapshot(snapshot));
    render_new_transcript_entries(&ui.transcript()[rendered.min(ui.transcript().len())..]);
    Ok(())
}

/// Print transcript entries this tick's drain just produced. Raw mode (held
/// for the whole interactive session, see `TerminalGuard`) disables the
/// terminal's own `\n` -> `\r\n` translation, so every line is written with
/// an explicit `\r\n` — a bare `println!` here would stair-step down the
/// screen instead of returning to column 0. This is deliberately plain text,
/// not a rendered transcript panel (`crates/tui`'s fuller panel/view-model
/// surface isn't wired into `apps/rapid` — see this module's own doc
/// comment) — a real next step, not an oversight.
fn render_new_transcript_entries(entries: &[tui::state::TranscriptEntry]) {
    use std::io::Write as _;
    use tui::state::{ToolActivityStatus, TranscriptEntry};
    let mut out = io::stdout();
    for entry in entries {
        let line = match entry {
            TranscriptEntry::User { text } => format!("> {text}"),
            TranscriptEntry::Assistant { text } => text.clone(),
            TranscriptEntry::ToolActivity { tool, status } => {
                let marker = match status {
                    ToolActivityStatus::Started => "→",
                    ToolActivityStatus::Completed => "✓",
                    ToolActivityStatus::Failed => "✗",
                    ToolActivityStatus::Denied => "⛔",
                    ToolActivityStatus::ApprovalRequired => "⏸",
                };
                format!("{marker} {tool}")
            }
            TranscriptEntry::TurnFailed { reason } => format!("(turn failed: {reason})"),
            TranscriptEntry::TurnInterrupted => "(interrupted)".to_owned(),
        };
        for physical_line in line.split('\n') {
            let _ = write!(out, "{physical_line}\r\n");
        }
    }
    let _ = out.flush();
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
            Self::Config(_) | Self::Command(_) => JsonlExitCode::Usage.as_i32(),
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
            Self::Command(err) => write!(f, "{err}"),
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
}
