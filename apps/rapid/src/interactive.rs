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
use crate::host::{ExecOutcome, PreservedLiveContext, StepDiag, UnconfiguredModel, run_live_exec};
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
usage: rapid exec <prompt> [--verbose]

Run one headless agent turn with the configured model. The final response is
printed to stdout; diagnostics go to stderr; a non-zero exit code reports a
failed turn.

Arguments:
  <prompt>    Task prompt for the agent (required)

Options:
  --verbose   Per-attempt model and turn diagnostics on stderr
  -h, --help  Print this help

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
        Some("exec") => exec_turn(&args[1..]),
        Some("goal") => run_goal_command(&args[1..]),
        Some("playbook-compile") => p9(&args[1..], crate::p9_commands::run_playbook_compile),
        Some("mcp-tools") => p9(&args[1..], crate::p9_commands::run_mcp_tools),
        Some("agent-cli") => p9(&args[1..], crate::p9_commands::run_agent_cli),
        Some("doctor") => p9(&args[1..], crate::p9_commands::run_doctor),
        Some("sessions") => p9(&args[1..], crate::p9_commands::run_sessions),
        Some("inspect-export") => p9(&args[1..], crate::p9_commands::run_inspect_export),
        Some("cron") => p9(&args[1..], crate::p9_commands::run_cron),
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
            return Ok(1);
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
        return Ok(1);
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
                return Ok(1);
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
                return Ok(1);
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
            Ok(if outcome.accepted { 0 } else { 1 })
        }
        "export" => {
            let Some(export) = host.export(&cancel) else {
                println!("no active goal");
                return Ok(1);
            };
            println!("{export}");
            Ok(0)
        }
        "verify" => {
            if host.snapshot().is_none() {
                println!("no active goal");
                return Ok(1);
            }
            let allowed = host.can_complete(&cancel);
            println!("complete: {allowed}");
            if let Some(verdicts) = host.validate(&cancel) {
                for verdict in verdicts.verdicts() {
                    if verdict.satisfied() {
                        println!("- criterion {}: satisfied", verdict.criterion_id());
                    } else {
                        let reason = verdict
                            .reason()
                            .map(|r| r.as_str())
                            .unwrap_or("unsatisfied");
                        println!(
                            "- criterion {}: unsatisfied ({reason})",
                            verdict.criterion_id()
                        );
                    }
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
        return Ok(1);
    }
    if let Err(err) = host.save_evidence(&evidence_path) {
        eprintln!("{err}");
        return Ok(1);
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
        return Ok(1);
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
        return Ok(1);
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
        return Ok(1);
    };
    if kind == "complete" && !host.can_complete(cancel) {
        println!("completion refused: criteria are not all satisfied by recorded evidence");
        return Ok(1);
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
}

fn parse_exec_args(args: &[String]) -> Option<ExecArgs> {
    let mut verbose = false;
    let mut words: Vec<&str> = Vec::new();
    for arg in args {
        if arg == "--verbose" {
            verbose = true;
        } else {
            words.push(arg);
        }
    }
    let prompt = words.join(" ");
    if prompt.is_empty() {
        return None;
    }
    Some(ExecArgs { prompt, verbose })
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

/// Env var overriding the permission mode for one exec run.
const PERMISSION_MODE_ENV: &str = "RAPIDLM_PERMISSION_MODE";
/// Project settings documents consulted for the permission lattice, in
/// precedence order (RapidLM's own first, then the Claude-compat path).
const PROJECT_SETTINGS_FILES: [&str; 2] = [".rapidlm/settings.json", ".claude/settings.json"];
/// Persisted per-project allow grants consulted before any ask.
const PERMISSIONS_STORE_NAME: &str = "project-permissions.json";
/// Maximum rule entries admitted across all settings documents.
const MAX_WIRED_RULES: usize = crate::permissions::MAX_RULES;

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

fn exec_permission_lattice(
    canonical_root: Option<&Path>,
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
    let mut rules: Vec<crate::permissions::ToolRule> = Vec::new();
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
    for settings in &loaded_settings {
        for rule in settings.rules.clone() {
            if rules.len() >= MAX_WIRED_RULES {
                break;
            }
            rules.push(rule);
        }
    }
    let mode = mode.unwrap_or(PermissionMode::Default);
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
    Ok(lattice)
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
}

impl crate::exec_tools::SubagentRunner for LiveSubagentRunner {
    fn run(&self, prompt: &str, agent_type: &str) -> Result<String, String> {
        use crate::exec_tools::ExecTools;
        let store = auth::InMemoryCredentialStore::new();
        let model = crate::model::ConfiguredModel::build(&self.active, &store)
            .map_err(|err| err.to_string())?;
        let mut tools = if agent_type == "explore" || agent_type == "plan" {
            ExecTools::read_only_with_permissions(&self.root, self.permissions.clone())
        } else {
            ExecTools::workspace_with_permissions(&self.root, self.permissions.clone())
        }
        .map_err(|err| err.to_string())?;
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
            &agent_runtime::CancellationToken::new(),
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
        Ok(summary)
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
fn exec_turn(args: &[String]) -> Result<i32, InteractiveError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{EXEC_USAGE}");
        return Ok(0);
    }
    let Some(parsed) = parse_exec_args(args) else {
        eprint!("{EXEC_USAGE}");
        return Err(InteractiveError::Usage);
    };
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
    let request = AgentExecutionRequest::new(spec, protocol::SessionId::new());
    let cancel = agent_runtime::CancellationToken::new();
    let mut events: Vec<agent_runtime::TurnEvent> = Vec::new();

    // Tools stay fail-closed: workspace tools are granted only when the
    // project is explicitly trusted and its root still resolves, and every
    // call then passes the six-mode permission lattice (deny rules, persisted
    // grants, mode table; headless asks become typed denials). Every other
    // surface refuses all proposed tool calls. Resolved once here and reused
    // below (subagent tools, the refuses-all warning) instead of re-resolving
    // the mode/env at every call site.
    let permission_lattice =
        match exec_permission_lattice(workspace.as_ref().map(|(root, _)| root.as_path())) {
            Ok(lattice) => lattice,
            Err(reason) => {
                eprintln!("permission configuration error: {reason}");
                return Ok(1);
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
    if let (Some((root, TrustStatus::Trusted)), ExecTools::Workspace(_)) =
        (&workspace, &mut tools)
    {
        let settings_path = root.join(".rapidlm/settings.json");
        if let Ok(text) = fs::read_to_string(&settings_path)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let allowlist: Vec<String> = value
                .get("fetch_allowlist")
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            tools.set_fetch_allowlist(allowlist);
            if let Some(hooks) = crate::hooks::HooksConfig::parse(&value) {
                if !hooks.is_empty() {
                    tools.set_hooks(hooks);
                }
            }
            let servers = crate::exec_tools::parse_mcp_servers(&value);
            if !servers.is_empty() {
                tools.register_mcp_servers(&servers);
            }
        }
    }

    // Layered model selection (env overrides > user config > typed fallback).
    // The store outlives the model, which borrows it for the router resolver.
    let credential_store = auth::InMemoryCredentialStore::new();
    let mut base_url = String::from("unconfigured");
    let mut child_model_config: Option<crate::user_config::ActiveModel> = None;
    let backing = match crate::user_config::select_from_process_env_gated() {
        Ok(ModelSelection::Configured { active, warnings }) => {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            base_url = active.entry.base_url.clone();
            child_model_config = Some(active.as_ref().clone());
            match ConfiguredModel::build(
                &apply_reminder_floor(*active, reminder_floor),
                &credential_store,
            ) {
                Ok(model) => SelectedModel::Configured(Box::new(model)),
                Err(err) => {
                    eprintln!("model configuration error: {err}");
                    return Ok(1);
                }
            }
        }
        Ok(ModelSelection::Unconfigured { .. }) => {
            eprintln!("{NOT_CONFIGURED_HINT}");
            SelectedModel::Unconfigured(UnconfiguredModel)
        }
        Err(err) => {
            eprintln!("model configuration error: {err}");
            return Ok(1);
        }
    };
    // Subagents: with a configured model, task_spawn runs child agents with
    // the same provider config and a depth-restricted read-only-capable tool
    // surface. Memory index: .rapidlm/MEMORY.md is always loaded (bounded).
    if let (Some(active), Some((root, TrustStatus::Trusted))) =
        (child_model_config.as_ref(), workspace.as_ref())
    {
        tools.set_subagent_runner(std::sync::Arc::new(LiveSubagentRunner {
            active: active.clone(),
            root: root.clone(),
            permissions: permission_lattice.clone(),
        }));
    }
    let diag = parsed.verbose.then(|| StepDiag::stderr(&base_url));
    match run_live_exec(
        preserved,
        backing,
        &request,
        &mut tools,
        &mut events,
        &cancel,
        ContextRetryPolicy::default(),
        diag,
    ) {
        Ok(outcome) if outcome.result.status() == AgentTerminalStatus::Succeeded => {
            println!("{}", outcome.result.summary());
            crate::exec_diag::stderr_line(&format!("tokens used: {}", outcome.tokens));
            Ok(0)
        }
        Ok(outcome) => {
            let mut message =
                describe_turn_failure(&outcome.result, outcome.failure_cause, outcome.failure_detail.as_ref());
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
                return Ok(0);
            }
            if outcome.failure_cause.is_none()
                && !matches!(&workspace, Some((_, TrustStatus::Trusted)))
            {
                message.push_str(" (workspace tools are disabled: project is not trusted)");
            }
            crate::exec_diag::stderr_line(&message);
            Ok(1)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
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

    let loop_result = SessionLoop {
        client: &client,
        stream: &mut stream,
        ui: &mut ui,
        session_id,
        actor: &actor,
        cancel: &options.cancel,
        interrupt_count: &mut interrupt_count,
        saw_ctrl_c: &mut saw_ctrl_c,
    }
    .run(&mut inputs);

    close_stream(&mut stream);
    let restore_ok = terminal.restore().is_ok() && terminal.is_restored();
    let _ = interrupt_session(&client, session_id, &actor, &options.cancel);
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
        self.submit_turn()?;
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
                self.submit_turn()?;
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

    fn submit_turn(&mut self) -> Result<(), InteractiveError> {
        if self.ui.actions_blocked() {
            return Ok(());
        }
        let expected_seq = self.ui.snapshot().map(|s| s.seq()).unwrap_or(0);
        block_on(
            self.client.submit_turn(SubmitTurn::new(
                self.session_id,
                expected_seq,
                self.actor.clone(),
                TraceId::new(),
            )),
            self.cancel,
        )?;
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
    let snapshot = block_on(client.get_session(session_id), cancel)?;
    *ui = reduce(ui.clone(), &UiEvent::Snapshot(snapshot));
    Ok(())
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
