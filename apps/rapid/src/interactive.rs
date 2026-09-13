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
use std::sync::LazyLock;
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
    MAX_OVERRIDE_ENTRIES, ProjectIdentity, ProjectTrustError, ProjectTrustStore, ServiceContext,
    ServiceError, ServiceFailureKind, ServiceGraph, ServiceId, ServiceStatus, SubmitTurn,
    SubscribeEvents, TrustStatus, config_key_from_env_name, load_config,
};
use protocol::{EventId, ProjectId, TraceContext, TraceId};
use tui::state::{
    GoalLifecycle, GoalProjection, LocalUiEvent, MAX_COMPOSER_BYTES, ToolActivityStatus,
    TranscriptEntry, UiEvent,
};
use tui::{
    AppState, CommandError, FrontendAction, FrontendKind, Inspector, KernelAction, KernelApi,
    LocalAction, PermissionsIntent, RecordingBackend, TerminalError, TerminalGuard, dispatch,
    parse_command_in, reduce,
};

use crate::exec_tools::ExecTools;
use crate::goal_host::{
    DriverLease, EVIDENCE_FILE, GOAL_FILE, GoalHost, GoalTransactionError, SESSIONS_DB_FILE,
    accrue_model_usage, accrue_turn_usage, active_goal_id, try_acquire_driver_lease,
};
use crate::headless::jsonl::JsonlExitCode;
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
pub(crate) const PROJECT_MARKER: &str = ".rapidlm";
pub(crate) const GIT_MARKER: &str = ".git";
const WORKSPACE_CONFIG_NAME: &str = "config.toml";
pub(crate) const USER_CONFIG_NAME: &str = "config.toml";
pub(crate) const TRUST_CATALOG_NAME: &str = "project-trust.json";
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
    Resize {
        width: u16,
        height: u16,
    },
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
    /// `--resume <id>` named a session this project's ledger has never seen.
    ///
    /// Distinct from a generic kernel error so the caller can name the id
    /// and offer the ids that *are* here, rather than surfacing a
    /// trace-id-bearing protocol error for what is almost always a typo or
    /// an id copied from another project.
    UnknownSession(protocol::SessionId),
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
    /// Resume this existing session instead of creating a new one.
    ///
    /// The kernel already has everything this needs — `get_session` for the
    /// projection and `subscribe(id, 0)` for a replay-then-tail of the whole
    /// event history — so a resumed session rebuilds its real transcript
    /// from the durable ledger rather than from any second store.
    pub resume: Option<protocol::SessionId>,
}

struct ResolvedProject {
    trust: TrustStatus,
    executable_config_active: bool,
    config: ConfigLoadResult,
    ledger_path: PathBuf,
    /// What [`adopt_legacy_ledger`] had to say, if anything — surfaced in
    /// the transcript rather than printed to stderr, which the alt-screen
    /// switch would wipe before the user could read it.
    ledger_notice: Option<String>,
    root: PathBuf,
    /// The RapidLM home this session resolved (`InteractiveOptions::
    /// user_home`, else its injected environment). Carried so a slash
    /// command cannot re-derive a *different* one from the process
    /// environment and then report trust from a catalog this session never
    /// consulted.
    user_home: PathBuf,
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
/// `rapid --help`.
///
/// Lists **only** subcommands [`SUBCOMMANDS`] actually dispatches. It used
/// to advertise eighteen further families — `run`, `resume`, `fork`,
/// `rewind`, `daemon`, `acp`, `graph`, `context`, `evidence`, `process`,
/// `computer`, `sandbox`, `hooks`, `skills`, `eval`, `inspect`, `export`,
/// `update` — that no dispatch arm ever matched, so typing one produced the
/// single line `usage: rapid [subcommand]` (`InteractiveError::Usage`'s own
/// `Display`, via `main`) and exit 2: byte-identical to a typo, with nothing
/// to say the command does not exist. They are a roadmap, and
/// `docs/reference/cli-command-reference.md` is where a roadmap belongs; a
/// `--help` that names a command the binary cannot run is simply wrong.
/// `cli_usage_lists_exactly_the_dispatched_subcommands` keeps this list and
/// the table in step in both directions.
/// `rapid --help`.
///
/// The command list is *derived* from [`SUBCOMMANDS`], not typed beside it.
/// It used to be a hand-written block, and it drifted from the table it
/// describes in every way a second list can: names that were dispatched but
/// undocumented, a `rapid goal … budget …` arm that never existed, and an
/// operand count (`rapid inspect-export <session>`) that documented an
/// invocation the parser rejects. Rendering removes the class of defect
/// rather than the instances.
pub static CLI_USAGE: LazyLock<String> = LazyLock::new(|| {
    let mut out = String::from(
        "usage: rapid [subcommand]\n\n\
         With no subcommand, rapid starts the interactive TUI in the current project.\n\n\
         Commands:\n",
    );
    for entry in SUBCOMMANDS {
        out.push_str(&render_subcommand_line(entry));
    }
    out.push_str(
        "\nEvery command above answers `--help`; four of them (exec, trust, mcp, doctor)\n\
         with full usage, the rest with a one-line summary.\n\
         New here? docs/getting-started.md covers install, first run, and exit codes.\n",
    );
    out
});

/// Column the summaries start at in [`CLI_USAGE`], chosen so the common
/// entries align inside an 80-column terminal.
const SUBCOMMAND_SUMMARY_COLUMN: usize = 34;

/// One `rapid --help` line: `  rapid <name> <operands>` padded to
/// [`SUBCOMMAND_SUMMARY_COLUMN`], then the summary.
///
/// An invocation too long for the column (`rapid goal`'s eleven
/// alternatives) puts its summary on the following line rather than pushing
/// it past the terminal's width, where wrapping would break the alignment
/// for everything after it — the same reason `/help`'s availability marker
/// is a leading prefix rather than a trailing one.
fn render_subcommand_line(entry: &Subcommand) -> String {
    let invocation = if entry.operands.is_empty() {
        format!("  rapid {}", entry.name)
    } else {
        format!("  rapid {} {}", entry.name, entry.operands)
    };
    // A single space would read as part of the invocation, so an entry that
    // reaches within one column of the summary wraps instead.
    if invocation.len() + 2 <= SUBCOMMAND_SUMMARY_COLUMN {
        let padding = SUBCOMMAND_SUMMARY_COLUMN - invocation.len();
        format!("{invocation}{:padding$}{}\n", "", entry.summary)
    } else {
        format!(
            "{invocation}\n{:SUBCOMMAND_SUMMARY_COLUMN$}{}\n",
            "", entry.summary
        )
    }
}

/// Exec-specific usage, printed by `rapid exec --help` and on exec usage
/// errors. Documents the prompt argument, the exec flags, and the env vars
/// that shape a headless run.
pub const EXEC_USAGE: &str = "\
usage: rapid exec <prompt> [--resume <session-id> | --continue] [--verbose]
                  [--max-wall-time <seconds>] [--json-schema <path>] [--jsonl]

Run one headless agent turn with the configured model. The final response is
printed to stdout; diagnostics go to stderr; a non-zero exit code reports a
failed turn. Each run is recorded as a session in this project's ledger;
`--resume`/`--continue` run the turn as the next turn of a recorded session
instead, so the model sees what was said before — the same history an
interactive turn sees, and the same session `rapid resume` reopens.

Arguments:
  <prompt>    Task prompt for the agent (required)

Options:
  --resume <session-id>   Continue this recorded session (an id `rapid
                          sessions list` or a previous run printed)
  --continue              Continue the session with the most recent activity
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
            print!("{}", *CLI_USAGE);
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
    *ui = reduce(
        ui.clone(),
        &UiEvent::Local(LocalUiEvent::SyncGoal(projection)),
    );
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

/// How a subcommand's handler reports failure. The two families exist
/// because `exec`/`trust`/`goal` are composition-root commands that already
/// speak [`InteractiveError`], while the `p9_commands` family speaks
/// [`crate::p9_commands::P9CommandError`] and is adapted by [`p9`].
enum SubcommandHandler {
    Native(fn(&[String]) -> Result<i32, InteractiveError>),
    P9(fn(&[String]) -> Result<i32, crate::p9_commands::P9CommandError>),
}

/// One dispatched subcommand: the name, the one-line summary `rapid --help`,
/// `rapid completions` and `rapid man` print, and the handler that runs it.
pub(crate) struct Subcommand {
    pub(crate) name: &'static str,
    /// What this command takes after its name, exactly as a user must type
    /// it: `<required>`, `[optional]`, `a|b|c` alternatives, or empty.
    ///
    /// Lives in the table because it was previously typed by hand into
    /// `CLI_USAGE` alone, and drifted: the help advertised `rapid
    /// inspect-export <session>` while the parser required a second
    /// positional, so the documented invocation could only ever fail — with
    /// an error pointing back at the same help. `CLI_USAGE`, `rapid
    /// <name> --help` and `rapid man` are all rendered from this now.
    pub(crate) operands: &'static str,
    pub(crate) summary: &'static str,
    /// Whether the handler recognises `--help`/`-h` itself. For the rest,
    /// [`run_subcommand`] answers centrally with the summary — `CLI_USAGE`
    /// tells the user to run `rapid <subcommand> --help`, and thirteen of
    /// these used to answer that with ``usage: see `rapid --help` `` and
    /// exit 2 (a literal loop), while `rapid playbook-compile --help` tried
    /// to read a *file* named `--help` and `rapid sessions --help` ignored
    /// the flag and ran.
    own_help: bool,
    handler: SubcommandHandler,
}

/// **The** subcommand table: dispatch and documentation from one list.
///
/// Previously these were two lists — a `match` in [`run_subcommand`] and a
/// separate `RAPID_SUBCOMMANDS` const — and they had drifted apart in both
/// directions: `trust`, `scan`, `insights` and `release-manifest` were
/// dispatched but absent from the help and the shell completions, while
/// `CLI_USAGE` advertised eighteen families (`daemon`, `acp`, `graph`,
/// `context`, `evidence`, `process`, `computer`, `sandbox`, `hooks`,
/// `skills`, `eval`, `inspect`, `export`, `update`, `run`, `resume`, `fork`,
/// `rewind`) that no arm ever matched, so each exited 2 with the single line
/// `usage: rapid [subcommand]` — output identical to a typo. A single table
/// makes the first class of drift impossible;
/// `cli_usage_lists_exactly_the_dispatched_subcommands` closes the second.
pub(crate) const SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "exec",
        operands: "<prompt>",
        summary: "one-shot/headless agent turn",
        own_help: true,
        handler: SubcommandHandler::Native(exec_subcommand),
    },
    Subcommand {
        name: "trust",
        operands: "grant|status|revoke",
        summary: "explicit project-trust control plane",
        own_help: true,
        handler: SubcommandHandler::Native(run_trust_command),
    },
    Subcommand {
        name: "resume",
        operands: "[session-id]",
        summary: "reopen the TUI on an existing session",
        own_help: true,
        handler: SubcommandHandler::Native(run_resume_command),
    },
    Subcommand {
        name: "goal",
        operands: "create|replace|show|pause|resume|cancel|complete|claim|export|verify|evidence",
        summary: "durable goal lifecycle",
        own_help: false,
        handler: SubcommandHandler::Native(run_goal_command),
    },
    Subcommand {
        name: "playbook-compile",
        operands: "<file.json>",
        summary: "compile a playbook into a graph",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_playbook_compile),
    },
    Subcommand {
        name: "mcp",
        operands: "list|get|add|remove|probe",
        summary: "project MCP servers (stdio)",
        own_help: true,
        handler: SubcommandHandler::P9(crate::p9_commands::run_mcp),
    },
    Subcommand {
        name: "mcp-tools",
        operands: "",
        summary: "the published RapidLM MCP server surface",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_mcp_tools),
    },
    Subcommand {
        name: "tools",
        operands: "",
        summary: "model-facing tool surface as JSON schemas",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_tools_schema),
    },
    Subcommand {
        name: "agent-cli",
        operands: "<prompt> -- argv...",
        summary: "one supervised external CLI agent turn",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_agent_cli),
    },
    Subcommand {
        name: "permissions",
        operands: "list|allow|revoke",
        summary: "persisted per-project tool grants",
        own_help: true,
        handler: SubcommandHandler::P9(crate::p9_commands::run_permissions),
    },
    Subcommand {
        name: "doctor",
        operands: "",
        summary: "config/model/trust/sandbox diagnosis (offline)",
        own_help: true,
        handler: SubcommandHandler::P9(crate::p9_commands::run_doctor),
    },
    Subcommand {
        name: "sessions",
        operands: "list|search",
        summary: "session projection over the event ledger",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_sessions),
    },
    Subcommand {
        name: "inspect-export",
        operands: "<session> <out-path> [--format jsonl|md|html]",
        summary: "export a session's event ledger",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_inspect_export),
    },
    Subcommand {
        name: "insights",
        operands: "<session>",
        summary: "report a session's insights projection",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_insights),
    },
    Subcommand {
        name: "cron",
        operands: "add|list|remove|poll",
        summary: "durable prompt cron (claim-lease firing)",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_cron),
    },
    Subcommand {
        name: "scan",
        operands: "",
        summary: "run the configured external scanners",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_scan),
    },
    Subcommand {
        name: "findings",
        operands: "list|dismiss",
        summary: "persisted scanner findings",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_findings),
    },
    Subcommand {
        name: "agents",
        operands: "list|validate|scaffold",
        summary: "project agent definitions",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_agents),
    },
    Subcommand {
        name: "plugins",
        operands: "validate|register|list|approve|reject|hook-test",
        summary: "plugin trust lifecycle",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_plugins),
    },
    Subcommand {
        name: "release-manifest",
        operands: "<version> <artifact>...",
        summary: "emit a release manifest of artifact digests",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_release_manifest),
    },
    Subcommand {
        name: "completions",
        operands: "bash|zsh|fish",
        summary: "emit shell completions",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_completions),
    },
    Subcommand {
        name: "man",
        operands: "",
        summary: "print the manual page text",
        own_help: false,
        handler: SubcommandHandler::P9(crate::p9_commands::run_man),
    },
];

/// What `rapid <not-a-command>` says. Split out because the only assertion a
/// test can make about `run_subcommand` itself is on its return value, which
/// is `Err(InteractiveError::Usage)` both before and after this message
/// existed — so a test that called it would pass whether or not the message
/// is emitted.
fn unknown_subcommand_text(name: &str) -> String {
    format!("rapid: unknown subcommand '{name}'")
}

/// `rapid resume [session-id]`: reopen the TUI on an existing session.
///
/// Everything this needs was already built and unwired: the durable
/// per-project event ledger, `get_session` for the projection, and
/// `subscribe(id, 0)`'s replay-then-tail for the history. With no id it
/// picks the session with the most recent *activity* — the one a user means
/// by "where I left off", which is not necessarily the one created last.
///
/// Sessions are keyed inside the project's own `.rapidlm` ledger, so an id
/// from another project is simply not found here; there is no cross-project
/// lookup to leak.
fn run_resume_command(args: &[String]) -> Result<i32, InteractiveError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{RESUME_USAGE}");
        return Ok(0);
    }
    if args.len() > 1 {
        eprintln!("rapid resume: unexpected argument '{}'", args[1]);
        eprint!("{RESUME_USAGE}");
        return Err(InteractiveError::Usage);
    }
    let mut options = InteractiveOptions::from_env()?;
    let resolved = resolve_project(&options)?;
    let ledger_path = resolved.ledger_path.clone();

    let session = match args.first() {
        Some(raw) => raw.parse::<protocol::SessionId>().map_err(|_| {
            eprintln!("rapid resume: {raw:?} is not a session id");
            InteractiveError::Usage
        })?,
        None => match most_recent_session(&ledger_path)? {
            Some(session) => session,
            None => {
                eprintln!(
                    "rapid resume: no session has been recorded in this project yet; \
run `rapid` to start one"
                );
                return Err(InteractiveError::Usage);
            }
        },
    };
    options.resume = Some(session);
    let report = match run_interactive(options) {
        Err(err @ InteractiveError::UnknownSession(_)) => {
            eprintln!("{err}");
            if let Some(hint) = known_sessions_hint(&ledger_path) {
                eprint!("{hint}");
            }
            return Err(InteractiveError::Usage);
        }
        other => other?,
    };
    Ok(report.outcome.exit_code())
}

/// How many session ids an unknown-id failure offers back.
const MAX_HINTED_SESSIONS: usize = 10;

/// The ids actually recorded in this project, most recent activity first,
/// for the "you asked for a session that isn't here" path.
///
/// This reads the interactive ledger directly rather than deferring to
/// `rapid sessions list`, which reads a *different* database
/// (`.rapidlm/sessions.sqlite`, the goal-evidence and cron store) and so
/// cannot see interactive sessions at all. Pointing a confused user at a
/// command that will print nothing would be worse than printing nothing
/// here.
fn known_sessions_hint(ledger_path: &Path) -> Option<String> {
    hint_lines(recorded_sessions(ledger_path).ok()?)
}

/// Render the hint from summaries already read.
///
/// Only rows whose id actually parses are offered, and each is printed in
/// its canonical form: an id this cannot parse is one `rapid resume` could
/// not accept either, so listing it would send the user in a circle. That
/// also keeps a ledger row from reaching the terminal verbatim — this writes
/// to stderr, where an escape sequence in a crafted `.rapidlm/ledger.sqlite`
/// would otherwise be interpreted rather than shown, and the ledger of a
/// project is exactly as trustworthy as the project. `last_activity` is a
/// timestamp string, so it is filtered to printable characters for the same
/// reason.
fn hint_lines(mut sessions: Vec<event_ledger::ledger::SessionSummary>) -> Option<String> {
    sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    let usable: Vec<(protocol::SessionId, String)> = sessions
        .into_iter()
        .filter_map(|summary| {
            let id = summary.session_id.parse::<protocol::SessionId>().ok()?;
            Some((id, printable(&summary.last_activity)))
        })
        .collect();
    if usable.is_empty() {
        return None;
    }
    let mut out = String::from("sessions recorded in this project:\n");
    for (id, last_activity) in usable.iter().take(MAX_HINTED_SESSIONS) {
        out.push_str(&format!("  {id}  last activity {last_activity}\n"));
    }
    if usable.len() > MAX_HINTED_SESSIONS {
        out.push_str(&format!(
            "  ... and {} more\n",
            usable.len() - MAX_HINTED_SESSIONS
        ));
    }
    Some(out)
}

/// Control characters replaced with spaces, for a stored string on its way
/// to a terminal. Mirrors `mcp_config::label`'s reasoning.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Every session this project's ledger has recorded.
///
/// One reader, shared by the "resume where I left off" default and the
/// unknown-id hint, so the two can never disagree about what this project
/// contains.
fn recorded_sessions(
    ledger_path: &Path,
) -> Result<Vec<event_ledger::ledger::SessionSummary>, InteractiveError> {
    if !ledger_path.exists() {
        return Ok(Vec::new());
    }
    let ledger =
        event_ledger::ledger::EventLedger::open(ledger_path).map_err(|_| InteractiveError::Io)?;
    ledger
        .list_sessions(&event_ledger::ledger::CancellationToken::new())
        .map_err(|_| InteractiveError::Io)
}

/// The session with the most recent event in this project's ledger.
///
/// `last_activity`, not `first_seen`: a session created yesterday and worked
/// in today is the one "resume where I left off" means.
fn most_recent_session(
    ledger_path: &Path,
) -> Result<Option<protocol::SessionId>, InteractiveError> {
    Ok(newest_usable(recorded_sessions(ledger_path)?))
}

/// The most recently active session that `rapid resume` could actually
/// accept.
///
/// Parses first and *then* takes the maximum: taking the newest row and
/// parsing it afterwards would let one corrupt row report "no session
/// recorded in this project" while several resumable ones sit behind it.
fn newest_usable(
    sessions: Vec<event_ledger::ledger::SessionSummary>,
) -> Option<protocol::SessionId> {
    sessions
        .into_iter()
        .filter_map(|summary| {
            let id = summary.session_id.parse::<protocol::SessionId>().ok()?;
            Some((id, summary.last_activity))
        })
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(id, _)| id)
}

/// `rapid resume --help`.
pub const RESUME_USAGE: &str = "usage: rapid resume [session-id]

Reopen the interactive TUI on an existing session, rebuilding its transcript
from this project's durable event ledger.

With no id, the session with the most recent activity is resumed. Naming an
id this project has never recorded lists the ids it does have; a session
from another project is not among them, since the ledger lives under the
project root.

  -h, --help  Print this help
";

/// `rapid exec` needs the extra `None` the other native handlers do not, so
/// it gets the table's one adapter rather than the table growing a shape for
/// a single entry.
fn exec_subcommand(args: &[String]) -> Result<i32, InteractiveError> {
    exec_turn(args, None)
}

fn run_subcommand(args: &[String]) -> Result<i32, InteractiveError> {
    // `classify_launch` deliberately looks past leading flags to decide
    // this is a subcommand launch at all (`rapid --jsonl exec hi`), so the
    // name is the first *non-flag* word. Reading `args.first()` blindly
    // reported `--jsonl` — and, worse, `--help` — as an unknown subcommand.
    let Some(index) = args.iter().position(|arg| !arg.starts_with('-')) else {
        return Err(InteractiveError::Usage);
    };
    let args = &args[index..];
    let name = args[0].as_str();
    let Some(entry) = SUBCOMMANDS.iter().find(|entry| entry.name == name) else {
        // Naming the offending word matters: the previous behavior emitted
        // only `usage: rapid [subcommand]` (`InteractiveError::Usage`'s
        // `Display`, printed by `main`), so a real typo and one of the
        // eighteen advertised-but-absent families were indistinguishable.
        eprintln!("{}", unknown_subcommand_text(name));
        return Err(InteractiveError::Usage);
    };
    let operands = &args[1..];
    if !entry.own_help && operands.iter().any(|arg| arg == "--help" || arg == "-h") {
        // Answered here rather than left to the handler: `CLI_USAGE`
        // promises `rapid <subcommand> --help` works, and these handlers
        // either reject the flag as a usage error, ignore it, or treat it as
        // an operand. A one-line summary is thin help, but it is true and it
        // exits 0.
        // The operands come from the same field `rapid --help` renders, so
        // a command's two help surfaces cannot disagree about what it takes.
        if entry.operands.is_empty() {
            println!("rapid {}: {}", entry.name, entry.summary);
        } else {
            println!(
                "usage: rapid {} {}\n\n{}",
                entry.name, entry.operands, entry.summary
            );
        }
        println!("see `rapid --help` for the full command list");
        return Ok(0);
    }
    match entry.handler {
        SubcommandHandler::Native(handler) => handler(operands),
        SubcommandHandler::P9(handler) => p9(operands, handler),
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
            let before = store
                .get(&identity, &cancel)
                .map_err(InteractiveError::Trust)?;
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
            let before = store
                .get(&identity, &cancel)
                .map_err(InteractiveError::Trust)?;
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
            let status = store
                .get(&identity, &cancel)
                .map_err(InteractiveError::Trust)?;
            println!("{}: {root_display}", status.as_str());
            Ok(0)
        }
        _ => unreachable!("validated above"),
    }
}

/// Every `rapid goal` subcommand [`run_goal_command`] accepts.
///
/// Exists for the same reason [`SUBCOMMANDS`] does, one level down:
/// `CLI_USAGE` advertised `rapid goal … budget …`, which no arm has ever
/// matched, while omitting `replace` and `complete`, which do.
/// `cli_usage_lists_exactly_the_goal_subcommands` keeps the two in step.
pub(crate) const GOAL_SUBCOMMANDS: &[&str] = &[
    "create", "replace", "show", "pause", "resume", "cancel", "complete", "claim", "export",
    "verify", "evidence",
];

/// Durable host-owned goal contract: `goal create|show|pause|resume|cancel`.
/// The goal is persisted under `.rapidlm/goal.json` so lifecycle commands work
/// across invocations; completion still requires the evidence gate.
fn run_goal_command(args: &[String]) -> Result<i32, InteractiveError> {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("usage: rapid goal <{}> ...", GOAL_SUBCOMMANDS.join("|"));
        return Err(InteractiveError::Usage);
    };
    // Checked before any file or ledger is opened, and against the same list
    // `CLI_USAGE` advertises, so an unknown name is named as unknown rather
    // than reaching the match's fallback and looking like bad arguments to a
    // real subcommand.
    if !GOAL_SUBCOMMANDS.contains(&sub) {
        eprintln!(
            "rapid goal: unknown subcommand '{sub}' (expected one of: {})",
            GOAL_SUBCOMMANDS.join(", ")
        );
        return Err(InteractiveError::Usage);
    }
    let cancel = agent_runtime::CancellationToken::new();
    let path = project_path(GOAL_FILE);
    let evidence_path = project_path(EVIDENCE_FILE);
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
    let claim_ledger = match event_ledger::ledger::EventLedger::open(current_project_ledger_path())
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
                requirements.push(agent_runtime::EvidenceRequirement::new(id, kinds).map_err(
                    |err| {
                        eprintln!("{err}");
                        InteractiveError::Usage
                    },
                )?);
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
            host.update(&path, |host| {
                host.apply(command, &GoalActor::Human, &cancel)
            })
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
            let claim = crate::goal_claim::GoalClaim::new(summary, checks, timeout_secs).map_err(
                |err| {
                    eprintln!("{err}");
                    InteractiveError::Usage
                },
            )?;
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
        // Unreachable for any name in `GOAL_SUBCOMMANDS`, which the guard
        // above already required; kept so adding a name to the const without
        // an arm degrades to a usage error rather than failing to compile
        // into a panic.
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
        flags
            .entry(key.to_owned())
            .or_insert(Vec::new())
            .push(value.clone());
        idx += 2;
    }
    Ok(flags)
}

/// First value of a flag, for single-value flags.
fn one<'a>(
    flags: &'a std::collections::BTreeMap<String, Vec<String>>,
    key: &str,
) -> Option<&'a str> {
    flags
        .get(key)
        .and_then(|values| values.first())
        .map(String::as_str)
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
    let subject = one(&flags, "subject").unwrap_or("goal").to_owned();
    let source_hash = protocol::ArtifactId::from_bytes(
        one(&flags, "source-hash").unwrap_or(&assertion).as_bytes(),
    );
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
    match (
        one(&flags, "session"),
        one(&flags, "seq"),
        one(&flags, "event-id"),
    ) {
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
    host.update_evidence(
        evidence_path,
        |host| -> Result<(), agent_runtime::EvidenceError> {
            let record = host.record_evidence(spec)?;
            println!(
                "recorded {} kind={} status={} producer={}",
                record.id(),
                record.kind(),
                record.status(),
                record.producer()
            );
            Ok(())
        },
    )
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
            record.ledger_ref().map(|_| " backed=ledger").unwrap_or(""),
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
        // The usage line cannot say which of several the user meant; the
        // count can, and the panel shows the names to pick from.
        CommandError::AmbiguousId { field, .. } => {
            format!("{err} — /{field} lists them; give more of the id")
        }
        CommandError::Empty | CommandError::NotACommand => String::new(),
    }
}

/// Token budget for a proactive-retrieval pass.
///
/// Shared by the turn path and `/context search` rather than written twice:
/// the panel's whole claim is that it shows the blocks a real turn would be
/// given for that prompt, and two constants that merely happen to match
/// today would make that claim quietly false the first time one moved.
const RETRIEVAL_BUDGET_TOKENS: u32 = 2048;

/// Local-error text for an [`Inspector`] the TUI has no route to open.
///
/// Same contract as [`unsupported_command_text`]: name the actual, specific
/// gap, and where a headless command already answers the question, name it
/// rather than leaving the user with "not available". Every command named
/// here is asserted to exist by
/// `every_command_an_unrouted_inspector_message_names_actually_exists`.
fn unrouted_inspector_text(inspector: &Inspector) -> String {
    let reason = match inspector {
        // Routed: handled by `dispatch_slash` before reaching this function.
        Inspector::Agents { .. }
        | Inspector::Diff { .. }
        | Inspector::Goal
        | Inspector::Context { .. }
        | Inspector::Memory
        | Inspector::Jobs { .. } => {
            "this inspector has a TUI route and should not reach this message"
        }
        // Handled by `open_unrouted_inspector` with a real report.
        Inspector::Mcp => "MCP configuration is reported inline and should not reach this message",
        Inspector::Knowledge => {
            "no knowledge-candidate store exists yet, so there is nothing to inspect"
        }
        Inspector::Playbook => {
            "playbooks compile (`rapid playbook-compile <file.json>`) but there is no \
name-addressable store to list them from"
        }
        Inspector::Trace => {
            "no TUI trace panel yet; export a session's event ledger with \
`rapid inspect-export <session> --format jsonl|md|html`"
        }
        Inspector::Insights => {
            "no TUI insights panel yet; `rapid insights <session>` runs the same \
`insights::analyze` over that session's events"
        }
        Inspector::Models => {
            "no TUI model panel yet; `rapid doctor` reports the resolved provider, model, \
fallback chain, credential source, and context budget"
        }
        Inspector::Plugins => {
            "no TUI plugin panel yet; `rapid plugins list` reports the trust catalog"
        }
        Inspector::Policy => {
            // `doctor::evaluate_security` never populates `policies`, so
            // doctor's own `security-policy` row is always `SKIP` with this
            // same reason — pointing a user at it would send them to a
            // command that reports the thing does not exist.
            "no user-authored capability-policy document exists in this build; the only policy \
stacks are in-process constants, and there is no TUI panel over them"
        }
        Inspector::Sandbox => {
            "no TUI sandbox panel yet; `rapid doctor` reports the selected backend and runs a \
real sandboxed smoke probe"
        }
        // Handled by `open_unrouted_inspector` with the real grant report.
        Inspector::Permissions => {
            "permissions are reported inline and should not reach this message"
        }
        Inspector::Computer => {
            "computer-use is not wired into the interactive session yet, so there is no state \
to inspect"
        }
    };
    format!("not available: {reason}")
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
        KernelAction::AddMcp { .. } => {
            "adding a server needs a program and its arguments, which `/mcp add` has no \
grammar for: run `rapid mcp add <name> --command <program>` (see `rapid mcp --help`)"
        }
        // Deliberately *not* wired, though `rapid mcp remove` exists and
        // would fit this grammar exactly. `KernelAction::requires_approval`
        // classifies every MCP mutation as approval-gated, and this build
        // has no approval broker — every other approval-gated action here
        // reports it is unavailable rather than acting. Wiring this one
        // would make it the first approval-classified action in the binary
        // that silently mutates the filesystem, and it edits
        // `.claude/settings.json`, a file another tool owns, from a
        // two-word slash command with no confirmation.
        KernelAction::RemoveMcp { .. } => {
            "removing a server is approval-gated and this build has no approval broker: run \
`rapid mcp remove <name>`, which is an explicit, argv-only command"
        }
        KernelAction::AuthMcp { .. } => {
            "no remote MCP transport is wired in this build — only stdio `command` servers \
are supported, and they have no auth step"
        }
        KernelAction::InstallPlugin { .. }
        | KernelAction::RemovePlugin { .. }
        | KernelAction::SetPluginPermissions { .. } => {
            "plugin install/trust management is not wired into the interactive session yet"
        }
        KernelAction::ApplyChangeSet { .. } | KernelAction::Rollback { .. } => {
            "no change-set apply/rollback backend exists yet"
        }
        KernelAction::Handoff { .. }
        | KernelAction::Takeover { .. }
        | KernelAction::ControlReturn => {
            "execution handoff/takeover is not wired into the interactive session yet"
        }
        KernelAction::ComputerObserve
        | KernelAction::ComputerRecord
        | KernelAction::ComputerTest => {
            "computer-use actions are not wired into the interactive session yet"
        }
        _ => "not available yet",
    };
    format!("not available: {reason}")
}

/// stderr guidance for the typed no-config fallback (mirrors the Grok Build
/// onboarding: a small user TOML selects provider, model, and credential).
pub(crate) const NOT_CONFIGURED_HINT: &str = "no model configured: add a [models] default and a [model.<id>] \
table (provider, model, base_url) to ~/.rapidlm/config.toml or point RAPIDLM_CONFIG at one; \
see docs/reference/model-configuration.md";

/// Load `.rapidlm/reminders.toml` and admit the always-on feeds. Returns the
/// rendered block plus the strongest reminder floor, or `None` when there is
/// no roster or nothing was admitted.
///
/// Takes the root the caller already resolved rather than resolving one of
/// its own: `exec_turn` passes the same `workspace` root it gives
/// `exec_permission_lattice`, so the reminders a turn honors always belong to
/// the project that turn is running in. `None` — the root did not resolve at
/// all, so there is no project — means no reminders, matching how every other
/// project-scoped input behaves on that path.
fn load_active_reminders(
    root: Option<&Path>,
) -> Result<
    Option<(String, agent_runtime::reminders::ReminderFloor)>,
    agent_runtime::reminders::ReminderError,
> {
    let Some(root) = root else {
        return Ok(None);
    };
    let path = root.join(PROJECT_MARKER).join("reminders.toml");
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
    Ok(active.render().map(|block| (block, active.effort_floor())))
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
) -> Result<
    (Vec<crate::user_config::ActiveModel>, Vec<String>),
    crate::managed_config::ManagedConfigError,
> {
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
    /// Run as the next turn of a recorded session rather than a new one.
    resume: ExecResume,
}

/// Which session a `rapid exec` turn is recorded in.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ExecResume {
    /// A fresh session, as every run before `--resume` existed.
    Fresh,
    /// `--resume <id>`: this session, which must be in the project's ledger.
    Session(protocol::SessionId),
    /// `--continue`: the session with the most recent activity.
    MostRecent,
}

fn parse_exec_args(args: &[String]) -> Option<ExecArgs> {
    let mut verbose = false;
    let mut max_wall_time = None;
    let mut json_schema = None;
    let mut jsonl = false;
    let mut resume = ExecResume::Fresh;
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--verbose" {
            verbose = true;
        } else if args[i] == "--resume" {
            i += 1;
            let id = args.get(i)?.parse::<protocol::SessionId>().ok()?;
            if resume != ExecResume::Fresh {
                return None;
            }
            resume = ExecResume::Session(id);
        } else if args[i] == "--continue" {
            if resume != ExecResume::Fresh {
                return None;
            }
            resume = ExecResume::MostRecent;
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
        resume,
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
pub(crate) fn exec_user_home() -> Option<PathBuf> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    user_home_from(&env)
}

/// [`exec_user_home`] against an explicit environment. Split out so
/// `rapid doctor` resolves the home directory through this exact precedence
/// (`RAPIDLM_HOME` names the home itself, `HOME`/`USERPROFILE` its parent)
/// rather than a doctor-local copy of the same three rules.
pub(crate) fn user_home_from(env: &[(String, String)]) -> Option<PathBuf> {
    if let Some(home) = env
        .iter()
        .find(|(key, _)| key == RAPIDLM_HOME_ENV)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
    {
        return canonicalize_or_create(Path::new(home)).ok();
    }
    for key in [HOME_ENV, USERPROFILE_ENV] {
        if let Some(home) = env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
            .filter(|value| !value.is_empty())
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
        Some(cause) => format!("{summary} ({}; {})", cause.as_str(), cause.remedy()),
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
pub(crate) const PROJECT_SETTINGS_FILES: [&str; 2] =
    [".rapidlm/settings.json", ".claude/settings.json"];
/// Persisted per-project allow grants consulted before any ask.
pub(crate) const PERMISSIONS_STORE_NAME: &str = "project-permissions.json";
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
        let settings = parse_settings(&text)
            .map_err(|err| format!("{} could not be loaded: {}", file_name, err.as_str()))?;
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

/// The persisted grants recorded for `root` in the store under `home`.
///
/// Split out of [`exec_permission_lattice`] purely so it can be driven with
/// an explicit home: the lattice builder resolves one from the process
/// environment via [`exec_user_home`], which a test cannot redirect without
/// mutating global state this crate forbids (`#![forbid(unsafe_code)]` rules
/// out `set_var`). Everything security-relevant is here; the caller only
/// supplies the home.
///
/// Fails closed in every failure mode — an absent, unreadable, or corrupt
/// store yields no grants, never "grant everything". `rapid permissions`
/// deliberately does *not* share that leniency: it refuses to overwrite a
/// store it could not parse.
pub(crate) fn persisted_grants_for(
    root: &Path,
    home: &Path,
) -> Vec<crate::permissions::ToolPattern> {
    let store_path = home.join(PERMISSIONS_STORE_NAME);
    let Ok(text) = fs::read_to_string(&store_path) else {
        return Vec::new();
    };
    let Ok(canonical) = fs::canonicalize(root) else {
        return Vec::new();
    };
    let Ok(grants) = crate::permissions::parse_grants(&text) else {
        return Vec::new();
    };
    grants.for_root(&canonical.to_string_lossy())
}

fn exec_permission_lattice(
    canonical_root: Option<&Path>,
    forced_mode: Option<crate::permissions::PermissionMode>,
) -> Result<crate::permissions::PermissionLattice, String> {
    use crate::permissions::{PermissionLattice, PermissionMode, ProjectSettings, parse_settings};
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
        let settings = parse_settings(&text)
            .map_err(|err| format!("{} could not be loaded: {}", file_name, err.as_str()))?;
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
    let (mode, gate_report) =
        crate::managed_config::gate_permission_mode(mode, managed_policy.as_ref());
    if let Some(report) = gate_report {
        eprintln!("warning: {report}");
    }
    let mut lattice = PermissionLattice::new(mode).with_rules(rules);
    // Persisted grants, keyed by canonical project root.
    if let (Some(root), Some(home)) = (canonical_root, exec_user_home()) {
        lattice = lattice.with_grants(persisted_grants_for(root, &home));
    }
    // Managed-policy tool ban (Modbit `CAP-001`, same layer as the mode
    // ceiling above): applied unconditionally, since a pure addition to
    // `denied_tools` has no lower-trust value to compare against — there is
    // no "allow_tools" override checked earlier that could widen past it.
    if let Some(patterns) = managed_policy
        .as_ref()
        .and_then(|policy| policy.denied_tools())
    {
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
    if let Some(scope) = managed_policy
        .as_ref()
        .and_then(|policy| policy.confine_writes_to())
    {
        eprintln!("warning: managed policy confines every write to '{scope}'");
        lattice = lattice.with_admin_write_scope(scope);
    }
    Ok(lattice)
}

/// A resolved project root plus which marker (if any) selected it.
pub(crate) struct FoundProject {
    pub(crate) root: PathBuf,
    pub(crate) marker: Option<&'static str>,
}

/// The exact chain every real command uses: `canonicalize_dir` then
/// `detect_project_root`. `detect_project_root` never reports "no project" —
/// it walks up to the nearest `.rapidlm`/`.git` marker or falls back to the
/// canonicalized cwd — so the marker is recorded separately to tell a real
/// project apart from a bare directory.
///
/// Lives here rather than in one command's own module so `rapid doctor` and
/// `rapid mcp` cannot drift into resolving different projects from the same
/// working directory.
pub(crate) fn resolve_project_root(
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<FoundProject, String> {
    let cwd = canonicalize_dir(cwd).map_err(|err| format!("{err}"))?;
    let root = detect_project_root(&cwd, cancel).map_err(|err| format!("{err}"))?;
    let marker = if root.join(PROJECT_MARKER).exists() {
        Some(PROJECT_MARKER)
    } else if root.join(GIT_MARKER).exists() {
        Some(GIT_MARKER)
    } else {
        None
    };
    Ok(FoundProject { root, marker })
}

/// The legacy name of the project event ledger, written only by the
/// interactive TUI before the two were unified.
const LEGACY_LEDGER_NAME: &str = "ledger.sqlite";

/// The project's one event ledger.
///
/// A project used to hold two SQLite databases with the *same* schema: the
/// TUI wrote `.rapidlm/ledger.sqlite` while every command that reads session
/// data (`rapid sessions`, `inspect-export`, `insights`) read
/// `.rapidlm/sessions.sqlite` — so `rapid sessions list` could never show an
/// interactive session, in the project that had just created one.
/// `sessions.sqlite` ([`SESSIONS_DB_FILE`]) is canonical: source already
/// documented it as such, and six command families already used it.
///
/// Resolution is pure and has no side effects, so a *reader* sees a
/// TUI-only project's history immediately, before anything is renamed:
/// canonical if it exists, else the legacy file if it exists, else canonical
/// (which is then created by whoever writes first). [`adopt_legacy_ledger`]
/// does the one-time tidy-up, and only from the writer.
pub(crate) fn project_ledger_path(marker_dir: &Path) -> PathBuf {
    let canonical = marker_dir.join(SESSIONS_DB_FILE);
    if canonical.exists() {
        return canonical;
    }
    let legacy = marker_dir.join(LEGACY_LEDGER_NAME);
    if legacy.exists() {
        return legacy;
    }
    canonical
}

/// Move a TUI-only project's ledger to the canonical name, once.
///
/// Returns a notice to show the user when anything happened worth telling
/// them, and `None` when there is nothing to say. Never merges and never
/// deletes: when *both* files exist the canonical one is already what
/// [`project_ledger_path`] returns, and the legacy file is left untouched and
/// named in the notice, because merging two ledgers is a real migration and
/// choosing silently — in either direction — would hide data the user has.
///
/// The ledger runs in WAL mode (`event_ledger::migrations` sets
/// `journal_mode = WAL` and verifies it), so a database with a live `-wal`
/// beside it is *not* one file: the sidecar can hold committed transactions
/// the main file does not have yet, and renaming the main file alone would
/// silently roll the ledger back to its last checkpoint. `EventLedger` holds
/// no persistent connection — it connects per operation — so opening it here
/// and letting the handle drop checkpoints the WAL and removes the sidecars,
/// after which the rename moves a complete database. Any sidecar still
/// present after that means someone else has the file open, and the adoption
/// is reported rather than forced.
pub(crate) fn adopt_legacy_ledger(marker_dir: &Path) -> Option<String> {
    let canonical = marker_dir.join(SESSIONS_DB_FILE);
    let legacy = marker_dir.join(LEGACY_LEDGER_NAME);
    if !legacy.exists() {
        return None;
    }

    if canonical.exists() {
        return Some(format!(
            "note: this project has two event ledgers. Using {}; {} is left untouched \
and its sessions are not listed. Nothing has been deleted.",
            canonical.display(),
            legacy.display()
        ));
    }
    // Checkpoint first: see this function's own note on WAL.
    if let Err(err) = event_ledger::ledger::EventLedger::open(&legacy) {
        return Some(format!(
            "note: {} could not be opened ({err}), so it was left where it is.",
            legacy.display()
        ));
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        if marker_dir
            .join(format!("{LEGACY_LEDGER_NAME}{suffix}"))
            .exists()
        {
            return Some(format!(
                "note: {} still has a {suffix} sidecar, so another process may have it \
open; it was left where it is and its sessions are still readable.",
                legacy.display()
            ));
        }
    }
    if let Err(err) = fs::rename(&legacy, &canonical) {
        return Some(format!(
            "note: {} could not be moved to {} ({err}); its sessions are still readable.",
            legacy.display(),
            canonical.display()
        ));
    }
    None
}

/// [`project_ledger_path`] for the project the working directory is in — the
/// one accessor every command that reads or writes session events uses, so
/// none of them can look in a different file than the TUI wrote.
pub(crate) fn current_project_ledger_path() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    project_ledger_path(&project_marker_dir_in(&cwd))
}

/// A path inside the current project's `.rapidlm`, resolved the way the TUI
/// resolves it./// A path inside the current project's `.rapidlm`, resolved the way the TUI
/// resolves it.
///
/// Every non-TUI command used to build these from the *working directory*
/// (`Path::new(".rapidlm").join(...)`) while `rapid` itself walked up to the
/// nearest marker. The two disagreed the moment a developer ran a command
/// from a subdirectory: `rapid goal show` in `src/` reported "no active
/// goal" for a project that had one, `rapid plugins` consulted a different
/// trust catalog than the one governing the project, and each such call left
/// a stray `.rapidlm/` behind in whatever directory it happened to run in.
/// `resolve_project_root`'s own doc comment already said it exists so
/// commands "cannot drift into resolving different projects from the same
/// working directory" — this is that guarantee applied to the rest of them.
///
/// Falls back to the cwd-relative path when the root cannot be resolved,
/// which is what every one of these call sites did unconditionally before,
/// so an unreadable cwd degrades to the old behavior rather than failing.
pub(crate) fn project_path(relative: impl AsRef<Path>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    project_path_in(&cwd, relative)
}

/// [`project_path`] with an explicit working directory, so the resolution
/// rule is testable without `chdir` (which is process-global and would race
/// every other test in this binary).
pub(crate) fn project_path_in(cwd: &Path, relative: impl AsRef<Path>) -> PathBuf {
    project_marker_dir_in(cwd).join(relative)
}

/// The `.rapidlm` directory of the project `cwd` is in, by the same rule.
pub(crate) fn project_marker_dir_in(cwd: &Path) -> PathBuf {
    let root = resolve_project_root(cwd, &CancellationToken::new())
        .map(|found| found.root)
        .unwrap_or_else(|_| cwd.to_path_buf());
    root.join(PROJECT_MARKER)
}

/// Trusted-project config merged from every file in `PROJECT_SETTINGS_FILES`
/// — the `web_fetch` allowlist, hooks, shadow-diagnostics config, and MCP
/// servers a `.rapidlm/settings.json` *or* `.claude/settings.json`-only
/// project can configure, in one place instead of duplicated at both call
/// sites (the real `exec_turn` path and this struct's own unit test).
pub(crate) struct ProjectIntegrations {
    fetch_allowlist: Vec<String>,
    pub(crate) hooks: crate::hooks::HooksConfig,
    shadow: Option<crate::shadow_diagnostics::ShadowDiagnosticsConfig>,
    /// Merged, deduplicated, project-wide-capped MCP configuration plus the
    /// entries that were *rejected* and why — see [`crate::mcp_config`],
    /// which owns every rule. This used to be a bare `Vec<McpServerConfig>`
    /// built by `extend`ing a per-file parse, which applied the server cap
    /// once per file, let two files each spawn a server of the same name
    /// (only the first of which was reachable), and dropped every invalid
    /// entry without a word.
    pub(crate) mcp: crate::mcp_config::McpProjectConfig,
}

/// Narrow the turn's disk/network/subagent ceilings to the managed policy's
/// (Modbit `CAP-001`/`WRK-017`) — narrow-only, so a missing or default
/// policy is a no-op — and return the policy's version for the router-
/// decision log. Re-loads the policy rather than threading it out of
/// `exec_permission_lattice`: that call already fails the whole turn closed
/// on an unreadable policy, so a load failure here means the file changed
/// underneath the turn, and skipping the (non-security-critical) narrowing
/// is safer than failing a turn whose lattice already resolved.
fn apply_managed_ceilings(tools: &mut ExecTools) -> Option<String> {
    let policy =
        crate::managed_config::load_policy(&std::env::vars().collect::<Vec<_>>()).ok()??;
    if let Some(max) = policy.max_write_bytes_per_turn() {
        tools.narrow_write_ceiling(max);
    }
    if let Some(max) = policy.max_fetch_bytes_per_turn() {
        tools.narrow_fetch_ceiling(max);
    }
    if let Some(max) = policy.max_subagent_spawns_per_turn() {
        tools.narrow_subagent_spawn_ceiling(max);
    }
    Some(policy.policy_version().to_owned())
}

/// The per-run hooks a trusted project declares, handed back by
/// [`configure_trusted_integrations`] for the caller to fire at its own
/// start and end — a headless run's, or an interactive session's.
#[derive(Default)]
struct SessionHooks {
    session_start: Vec<String>,
    session_end: Vec<String>,
}

/// What a trusted project's settings add to a turn's tools before any
/// model is known, the same for a headless run and an interactive turn:
/// the `web_fetch` allowlist, tool hooks, shadow diagnostics and MCP servers
/// (a configured server that will not run is reported through `warn`,
/// never dropped silently). Settings are merged across every file in
/// `PROJECT_SETTINGS_FILES` the same way `exec_permission_lattice` merges
/// permission rules. Runs before model resolution so a `session_start`
/// hook's output (an index, a memory file) is visible to the turn's
/// context, and so MCP start-up is not charged to a wall-time budget that
/// starts later.
///
/// The interactive turn used to get none of this — its doc comment listed
/// hooks, MCP and retrieval among what the "first working version" left
/// out — so the same `.rapidlm/settings.json` worked headless and silently
/// did nothing in the TUI.
fn configure_trusted_integrations(
    tools: &mut ExecTools,
    root: &Path,
    warn: &mut dyn FnMut(&str),
) -> SessionHooks {
    if !matches!(tools, ExecTools::Workspace(_)) {
        return SessionHooks::default();
    }
    let ProjectIntegrations {
        fetch_allowlist: allowlist,
        hooks: merged_hooks,
        shadow: shadow_config,
        mcp: mcp_config,
    } = load_project_integrations(root);
    tools.set_fetch_allowlist(allowlist);
    let session_hooks = SessionHooks {
        session_start: merged_hooks.session_start.clone(),
        session_end: merged_hooks.session_end.clone(),
    };
    if !merged_hooks.is_empty() {
        tools.set_hooks(merged_hooks);
    }
    if let Some(shadow) = shadow_config {
        tools.set_shadow_diagnostics(shadow);
    }
    for rejection in mcp_config.rejections() {
        // Bounded — see `mcp_config::MAX_REPORTED_REJECTIONS`: nothing
        // limits how many entries a settings file declares, and this runs
        // on every turn.
        warn(&format!(
            "warning: MCP server {:?} in {} not registered: {}",
            rejection.name, rejection.file, rejection.issue
        ));
    }
    if mcp_config.rejections_omitted() > 0 {
        warn(&format!(
            "warning: {} further MCP server(s) not registered; run `rapid mcp list` for the full report",
            mcp_config.rejections_omitted()
        ));
    }
    let mcp_servers = mcp_config.configs();
    if !mcp_servers.is_empty() {
        tools.register_mcp_servers(&mcp_servers);
    }
    session_hooks
}

/// The ledger observers a recorded turn's tools carry — background jobs to
/// `/jobs`, workspace writes to `/diff` — identified by where they record.
struct LedgerSinks<'a> {
    client: &'a InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &'a ActorRef,
}

/// What a trusted project adds to a turn's tools once the model is known:
/// the active model's own credential scrubbed from captured output, and the
/// subagent runner behind `task_spawn` (with a configured model). The
/// ledger sinks are attached here first, by construction: the runner
/// snapshots the tools' sinks for its children, so a runner built before
/// they were attached spawned children whose writes and jobs never reached
/// `/diff` or `/jobs` — which is exactly what the first interactive wiring
/// of this did.
fn configure_trusted_model_tools(
    tools: &mut ExecTools,
    root: &Path,
    active: Option<&crate::user_config::ActiveModel>,
    permission_lattice: &crate::permissions::PermissionLattice,
    sinks: Option<LedgerSinks<'_>>,
) {
    if let Some(sinks) = sinks {
        attach_ledger_sinks(tools, sinks.client, sinks.session_id, sinks.actor);
    }
    if !matches!(tools, ExecTools::Workspace(_)) {
        return;
    }
    // Scrub the active model's own resolved credential from captured
    // shell_exec output: a command that reads back a config file
    // containing it (a real, plausible thing to run, not a contrived
    // scenario — `~/.rapidlm/config.toml` stores it in plaintext) must not
    // hand it back to the model verbatim. Best-effort: a registration
    // failure (e.g. the credential is empty or oversized) just means
    // nothing gets scrubbed, not a turn failure.
    if let Some(plaintext) = active.and_then(|active| active.credential.plaintext.as_deref())
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
    // surface.
    if let Some(active) = active
        && let Some(turn_budgets) = tools.turn_budget_handles()
    {
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
            root: root.to_path_buf(),
            permissions: permission_lattice.clone(),
            turn_budgets,
            write_locks,
            job_budget,
            job_events: tools.job_events(),
            workspace_changes: tools.workspace_changes(),
            hooks,
            shadow_diagnostics,
            trace_calls,
            turn_ceilings,
            redaction: tools.redaction_handle(),
        }));
    }
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
pub(crate) fn load_project_integrations(root: &Path) -> ProjectIntegrations {
    let mut fetch_allowlist: Vec<String> = Vec::new();
    let mut hooks = crate::hooks::HooksConfig::default();
    let mut shadow = None;
    for file_name in PROJECT_SETTINGS_FILES {
        let settings_path = root.join(file_name);
        let Ok(text) = fs::read_to_string(&settings_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(entries) = value
            .get("fetch_allowlist")
            .and_then(serde_json::Value::as_array)
        {
            fetch_allowlist.extend(
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned)),
            );
        }
        if let Some(file_hooks) = crate::hooks::HooksConfig::parse(&value) {
            hooks.extend(file_hooks);
        }
        if shadow.is_none() {
            shadow = crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&value);
        }
    }
    // Each file's own HooksConfig::parse already capped itself at
    // MAX_HOOKS_PER_STAGE; re-cap after merging two files' worth so the
    // combined per-stage bound still holds.
    for stage in hooks.stages_mut() {
        stage.truncate(crate::hooks::MAX_HOOKS_PER_STAGE);
    }
    ProjectIntegrations {
        fetch_allowlist,
        hooks,
        shadow,
        // Its own loader: MCP config is the one integration whose bounds
        // are project-wide rather than per-file, so it cannot be merged by
        // the `extend`-per-file loop above without re-introducing exactly
        // the defects `mcp_config` exists to fix.
        mcp: crate::mcp_config::load_project_mcp(root),
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
    turn_budgets: (
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    ),
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
    /// The parent's job-event sink, propagated for the same reason the job
    /// *budget* is: a subagent's background jobs are this turn's jobs, and a
    /// `/jobs` panel showing only the parent's would be a half-truth about
    /// what is running.
    job_events: Option<std::sync::Arc<dyn crate::exec_tools::JobEvents>>,
    /// The parent's workspace-change sink, propagated for the same reason:
    /// a subagent's writes are this turn's writes, and a `/diff` showing
    /// only the parent's would be a half-truth about what changed.
    workspace_changes: Option<std::sync::Arc<dyn crate::exec_tools::WorkspaceChanges>>,
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
        if let Some(events) = self.job_events.clone() {
            tools.set_job_events(events);
        }
        if let Some(changes) = self.workspace_changes.clone() {
            tools.set_workspace_changes(changes);
        }
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
pub(crate) fn context_budget_for(backing: &SelectedModel<'_>) -> (u32, u32) {
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

/// How the `(context_limit, output_reserve)` pair [`context_budget_for`]
/// just returned was arrived at, as one human-readable phrase. Extracted
/// from `exec_turn`'s `--verbose` line so `rapid doctor`'s own
/// context-budget check reports the identical provenance instead of
/// re-deriving "is this the default or a configured value?" a second way.
///
/// Keyed off `backing` itself (what actually produced the numbers), not the
/// *primary's* raw config entry: the primary is not necessarily what backs a
/// resolved `Configured` model (the `backends.len() < 2`/controller-failure
/// paths in [`build_backing_model`] can promote a surviving alternate
/// instead), and a `FallbackChain`'s derived pair is a genuine cross-backend
/// combination — attributing it to "the primary's config" would misdescribe
/// where the numbers actually came from either way.
pub(crate) fn context_budget_source(backing: &SelectedModel<'_>, candidates: usize) -> String {
    match backing {
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
            "chain: minimum context_limit / maximum max_output across {candidates} candidates"
        ),
    }
}

/// The layered model *resolution* half of a turn's setup, as plain data:
/// env overrides > user config > typed no-config fallback, then the opt-in
/// `[models] fallback` chain resolved against the same config the primary
/// came from and narrowed by the same managed policy.
///
/// Extracted verbatim out of [`exec_turn`], which was its only caller, so
/// `rapid doctor` can answer "which model would this environment actually
/// use, and would it construct?" by running the real thing rather than a
/// doctor-specific reimplementation of the same precedence rules. Nothing
/// here performs network I/O or a provider request: `ConfiguredModel::build`
/// (the construction half, [`build_backing_model`]) validates endpoints and
/// bounds eagerly and locally.
///
/// `warn` receives every non-fatal line this resolution would otherwise
/// print directly — `exec_turn` passes a sink that `eprintln!`s immediately,
/// preserving its exact prior stderr ordering; doctor passes a collector.
pub(crate) struct ModelPlan {
    /// `[primary, ...fallback alternates]`, empty when unconfigured. The
    /// primary already carries the reminders' reasoning-effort floor.
    pub models: Vec<crate::user_config::ActiveModel>,
    /// The primary exactly as the config resolved it, *before* the reminder
    /// floor was applied — what a spawned child agent inherits.
    pub primary_config: Option<crate::user_config::ActiveModel>,
    /// The `[phases] compact` override, resolved and gated through the same
    /// managed policy as a fallback entry, when the config names one that
    /// differs from the primary. `None` means compaction runs on the
    /// conversation model.
    pub compact: Option<crate::user_config::ActiveModel>,
    /// No model config was found at all; the typed fallback applies.
    pub unconfigured: bool,
}

/// Typed failure of [`resolve_model_plan`]. `Display` reproduces the exact
/// stderr line `exec_turn` printed for each case before this was extracted.
#[derive(Debug)]
pub(crate) enum ModelPlanError {
    Config(String),
    ManagedPolicy(String),
}

impl std::fmt::Display for ModelPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "model configuration error: {reason}"),
            Self::ManagedPolicy(reason) => write!(f, "managed policy error: {reason}"),
        }
    }
}

pub(crate) fn resolve_model_plan(
    process_env: &[(String, String)],
    reminder_floor: agent_runtime::reminders::ReminderFloor,
    warn: &mut dyn FnMut(&str),
) -> Result<ModelPlan, ModelPlanError> {
    // `models`: [primary, ...fallback alternates] — resolved as plain data
    // (no borrows yet) so the credential-store count is known upfront.
    let mut models: Vec<crate::user_config::ActiveModel> = Vec::new();
    let mut primary_config: Option<crate::user_config::ActiveModel> = None;
    let mut compact: Option<crate::user_config::ActiveModel> = None;
    let mut unconfigured = false;
    match crate::user_config::select_active_model_gated(process_env) {
        Ok(ModelSelection::Configured { active, warnings }) => {
            for warning in warnings {
                warn(&format!("warning: {warning}"));
            }
            primary_config = Some(active.as_ref().clone());
            let primary = apply_reminder_floor(*active, reminder_floor);

            // Fallback chain: opt-in via `[models] fallback`, resolved
            // against the same raw config the primary came from, then
            // narrowed/raised by the same managed policy (if any) the
            // primary was already gated through — a fallback entry is never
            // let through a restriction, or under an effort floor, the
            // primary itself has to honor.
            if let Some(config) = crate::user_config::load_config(
                &crate::user_config::resolve_config_source(process_env),
            )
            .unwrap_or(None)
            {
                let (candidates, warnings) =
                    crate::user_config::resolve_fallback_chain(process_env, &config, &primary);
                for warning in warnings {
                    warn(&format!("warning: {warning}"));
                }
                let (gated, warnings) = gate_fallback_candidates(process_env, candidates)
                    .map_err(|err| ModelPlanError::ManagedPolicy(format!("{err}")))?;
                for warning in warnings {
                    warn(&format!("warning: {warning}"));
                }
                models.extend(gated);
                // `[phases] compact`: parsed and validated since the
                // phases table existed, honoured by nobody until now.
                // Resolved against the same raw config and gated the same
                // way a fallback entry is — never let through a
                // restriction the primary has to honour.
                let routed_elsewhere = primary
                    .phase_route
                    .override_for(llm_router::provider::ModelPurpose::Compact)
                    .is_some_and(|profile| profile.as_str() != primary.profile_id);
                if routed_elsewhere {
                    match crate::user_config::resolve_purpose_model(
                        process_env,
                        &config,
                        llm_router::provider::ModelPurpose::Compact,
                    ) {
                        Ok(candidate) => {
                            let (gated, warnings) =
                                gate_fallback_candidates(process_env, vec![candidate]).map_err(
                                    |err| ModelPlanError::ManagedPolicy(format!("{err}")),
                                )?;
                            for warning in warnings {
                                warn(&format!("warning: {warning}"));
                            }
                            compact = gated.into_iter().next();
                            if compact.is_none() {
                                warn(
                                    "warning: phases.compact is not on the managed provider allowlist; compaction runs on the conversation model",
                                );
                            }
                        }
                        Err(err) => warn(&format!(
                            "warning: phases.compact not resolved ({err}); compaction runs on the conversation model"
                        )),
                    }
                }
            }
            models.insert(0, primary);
        }
        Ok(ModelSelection::Unconfigured { .. }) => {
            warn(NOT_CONFIGURED_HINT);
            unconfigured = true;
        }
        Err(err) => return Err(ModelPlanError::Config(format!("{err}"))),
    }
    Ok(ModelPlan {
        models,
        primary_config,
        compact,
        unconfigured,
    })
}

/// Typed failure of [`build_backing_model`]. `Display` reproduces the exact
/// stderr line `exec_turn` printed for each case before this was extracted.
#[derive(Debug)]
pub(crate) struct BackingModelError(String);

impl std::fmt::Display for BackingModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model configuration error: {}", self.0)
    }
}

/// The *construction* half: turn a [`ModelPlan`]'s resolved entries into the
/// one [`SelectedModel`] a turn actually dispatches through, seeding one
/// already-built credential store per backend. Extracted out of [`exec_turn`]
/// alongside [`resolve_model_plan`] for the same reason — `rapid doctor` must
/// exercise the real adapter construction (endpoint parsing, capability
/// pinning, credential seeding), not assert that a config file "looks right".
///
/// `stores` must have exactly one entry per `models` entry and must already
/// be final: every returned borrow is a plain immutable borrow of it.
pub(crate) fn build_backing_model<'store>(
    models: &[crate::user_config::ActiveModel],
    stores: &'store [auth::InMemoryCredentialStore],
    unconfigured: bool,
    diag: Option<StepDiag>,
    policy_version: Option<String>,
    warn: &mut dyn FnMut(&str),
) -> Result<(SelectedModel<'store>, crate::host::RouterDecisionLog), BackingModelError> {
    // Routing-decision log (Modbit `MOD-005`: "routing must be auditable").
    // Only the fallback-chain branch below can ever produce a decision; every
    // other branch keeps an empty log — absence *is* the "no incident"
    // signal, not a missing feature.
    let mut router_decisions = crate::host::RouterDecisionLog::new();
    let backing = if unconfigured {
        SelectedModel::Unconfigured(UnconfiguredModel)
    } else if models.len() == 1 {
        match ConfiguredModel::build(&models[0], &stores[0]) {
            Ok(model) => SelectedModel::Configured(Box::new(model)),
            Err(err) => return Err(BackingModelError(format!("{err}"))),
        }
    } else {
        let mut backends = Vec::with_capacity(models.len());
        for (active, store) in models.iter().zip(stores.iter()) {
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
                llm_router::provider::ModelId::parse(&active.profile_id).expect(
                    "profile id already validated against the stricter llm-router alphabet",
                ),
            );
            match ConfiguredModel::build(active, store) {
                Ok(model) => backends.push((model_ref, model)),
                Err(err) => {
                    warn(&format!(
                        "warning: fallback entry '{}' failed to configure ({err}); skipped",
                        active.profile_id
                    ));
                }
            }
        }
        if backends.len() < 2 {
            // Every alternate failed to configure — fall back to the plain,
            // single-model path rather than a chain of one.
            match backends.into_iter().next() {
                Some((_, model)) => SelectedModel::Configured(Box::new(model)),
                None => {
                    return Err(BackingModelError(
                        "primary model failed to configure".to_owned(),
                    ));
                }
            }
        } else {
            let primary_ref = backends[0].0.clone();
            let alternate_refs: Vec<_> = backends[1..]
                .iter()
                .map(|(model_ref, _)| model_ref.clone())
                .collect();
            let policy = llm_router::fallback::FallbackPolicy::standard();
            let router_cancel = llm_router::provider::CancellationToken::new();
            match llm_router::fallback::FallbackController::from_explicit_chain(
                primary_ref,
                alternate_refs,
                policy,
                &router_cancel,
            ) {
                Ok(controller) => {
                    let mut chain = FallbackChainModel::new(backends, controller, diag);
                    // Reuses the single read captured by the caller (disk/
                    // network ceiling narrowing) rather than loading a third
                    // time — see that read's own doc comment for why: a third
                    // independent read could see a different file than the
                    // one that actually gated this turn's permission mode,
                    // and would make the recorded version describe a
                    // policy that wasn't the one actually applied.
                    chain.set_policy_version(policy_version);
                    router_decisions = chain.decisions();
                    SelectedModel::FallbackChain(Box::new(chain))
                }
                Err(err) => {
                    warn(&format!(
                        "warning: fallback chain configuration failed ({err}); using the primary model only"
                    ));
                    let (_, model) = backends.into_iter().next().expect("checked len >= 2 above");
                    SelectedModel::Configured(Box::new(model))
                }
            }
        }
    };
    Ok((backing, router_decisions))
}

/// project instructions discovered under `root`/`cwd`, and the
/// conditional-section system prompt (environment, trust posture, token
/// budget). Used by both the top-level `exec` turn and `task_spawn`
/// subagents so a child sees the same project rules and system prompt as its
/// parent instead of running with neither.
/// Ceiling on ledger events read back per session to reconstruct its
/// earlier turns for the model. A session longer than this still gets its
/// newest turns: the read starts far enough back to cover them, and
/// `PreservedLiveContext::with_conversation` keeps only the newest
/// `MAX_CONVERSATION_TURNS` anyway.
const MAX_CONVERSATION_EVENTS: usize = 8192;

/// How many forks back a conversation is followed. A forked session's own
/// ledger starts at `session.forked`; what was said before the fork is in
/// its parent, through `source_seq`, and in that parent's parent before
/// that. `/rewind` forks, so a rewound session remembers the turns up to
/// the point it was rewound to — and nothing after, which is the point.
const MAX_FORK_DEPTH: usize = 8;

/// The session's earlier turns, oldest first, read from the ledger's own
/// `turn.started`/`turn.completed|failed|interrupted` events — the record
/// of what was said, not the display transcript — following `session.forked`
/// back through the parents it branched from. A `turn.started` with no
/// terminal event yet is the turn being executed now (or one lost to a
/// crash) and is not carried; its prompt is the goal of this turn.
///
/// A `context.compacted` event folds everything before it: the turns read
/// so far are replaced by the summary it carries, and only the turns after
/// it are carried as turns. (Precisely: a turn recorded in the same session
/// after the event's `through_seq` is kept — the one way a turn can land
/// between the summary being written and the event being appended is
/// another process writing to the same ledger.)
///
/// Best-effort: a session that cannot be read back yields no history and
/// the turn runs with the prompt alone, as every turn did before this
/// existed. Nothing here can fail a turn.
fn conversation_history(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    cancel: &agent_runtime::CancellationToken,
) -> crate::host::ConversationHistory {
    conversation_history_within(client, session_id, cancel, MAX_CONVERSATION_EVENTS)
}

/// [`conversation_history`] with the read window as a parameter, so a test
/// can put a compaction outside it without appending thousands of events.
fn conversation_history_within(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    cancel: &agent_runtime::CancellationToken,
    window: usize,
) -> crate::host::ConversationHistory {
    let mut history = HistoryAccumulator::default();
    // The tip this read covers through, read once and named in the result:
    // a `/compact` records it as the summary's `through_seq`, so a turn
    // that lands after this read — only another process could — is
    // carried as a turn, whether it landed before or after the summary
    // was written.
    let kernel_cancel = CancellationToken::new();
    let Ok(snapshot) = block_on(client.get_session(session_id), &kernel_cancel) else {
        return crate::host::ConversationHistory::default();
    };
    let through_seq = snapshot.seq();
    conversation_through(
        client,
        session_id,
        Some((through_seq, snapshot.last_compaction())),
        cancel,
        MAX_FORK_DEPTH,
        window,
        &mut history,
    );
    crate::host::ConversationHistory {
        summary: history.summary,
        turns: history.turns.into_iter().map(|turn| turn.turn).collect(),
        through_seq,
    }
}

/// `conversation_through`'s working state: the turns read so far, each with
/// where its terminal event sits so a compaction can tell which of them it
/// covered, and the summary of the last compaction seen.
#[derive(Default)]
struct HistoryAccumulator {
    summary: Option<String>,
    turns: Vec<HistoryTurn>,
}

struct HistoryTurn {
    session: protocol::SessionId,
    seq: u64,
    turn: crate::host::ConversationTurn,
}

impl HistoryAccumulator {
    fn push(
        &mut self,
        session: protocol::SessionId,
        seq: u64,
        turn: crate::host::ConversationTurn,
    ) {
        self.turns.push(HistoryTurn { session, seq, turn });
    }

    /// Apply a `context.compacted` event recorded in `session` at
    /// `through_seq`: every turn from a parent session, and every turn of
    /// this session recorded through that seq, is now the summary.
    fn compact(&mut self, session: protocol::SessionId, through_seq: u64, summary: String) {
        self.turns
            .retain(|turn| turn.session == session && turn.seq > through_seq);
        self.summary = Some(summary);
    }
}

/// Append `session_id`'s turns through `through` — `(tip, latest
/// compaction seq)`, the session's own when `None` — to `history`, after
/// its fork parent's; recursion bounded by `depth`, the read by `window`.
///
/// A compaction older than the window is not lost: the projection
/// remembers where the latest one is (`SessionSnapshot::last_compaction`),
/// and that one event is read first, so the summary is carried even when
/// the turns between it and the window are not (they are older than
/// anything the window would carry anyway).
#[allow(clippy::too_many_arguments)]
fn conversation_through(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    through: Option<(u64, Option<u64>)>,
    cancel: &agent_runtime::CancellationToken,
    depth: usize,
    window: usize,
    history: &mut HistoryAccumulator,
) {
    use crate::host::{ConversationOutcome, ConversationTurn};
    use event_ledger::event::EventKind;
    use protocol::RedactionClass;
    // The kernel calls take the kernel's own token; the turn's token is the
    // one that can actually be cancelled, and the read loop watches it.
    let kernel_cancel = CancellationToken::new();
    let (tip, last_compaction) = match through {
        Some(through) => through,
        None => match block_on(client.get_session(session_id), &kernel_cancel) {
            Ok(snapshot) => (snapshot.seq(), snapshot.last_compaction()),
            Err(_) => return,
        },
    };
    let text_of = |event: &event_ledger::event::ErasedEventEnvelope, field: &str| {
        if event.redaction() == RedactionClass::Secret {
            return None;
        }
        event
            .payload()
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let fold = |history: &mut HistoryAccumulator,
                event: &event_ledger::event::ErasedEventEnvelope| {
        // Without a summary the event folds nothing: better the turns than
        // a hole where they were. The bound is the writer's
        // (`MAX_COMPACTION_SUMMARY`); text beyond it is not this build's,
        // and folding the turns on the strength of a summary the packet
        // will then refuse would be the same hole.
        let Some(summary) = text_of(event, "summary")
            .filter(|text| !text.is_empty() && text.len() <= crate::host::MAX_COMPACTION_SUMMARY)
        else {
            return;
        };
        let through_seq = event
            .payload()
            .get("through_seq")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(event.seq());
        history.compact(session_id, through_seq, summary);
    };
    let after = tip.saturating_sub(window as u64);
    // The latest compaction, when it is older than the window: read that
    // one event before the window, so its summary stands for everything
    // the window does not reach — including the parent's turns already
    // read.
    if let Some(seq) = last_compaction
        && seq <= tip
        && seq <= after
        && let Ok(mut stream) = block_on(
            client.subscribe(SubscribeEvents::new(session_id, seq.saturating_sub(1))),
            &kernel_cancel,
        )
        && let Ok(event) = stream.recv()
        && event.kind() == EventKind::ContextCompacted
    {
        fold(history, &event);
    }
    let Ok(mut stream) = block_on(
        client.subscribe(SubscribeEvents::new(session_id, after)),
        &kernel_cancel,
    ) else {
        return;
    };
    let mut open: Option<String> = None;
    for _ in 0..window {
        if stream.cursor() >= tip || cancel.is_cancelled() {
            break;
        }
        let Ok(event) = stream.recv() else {
            break;
        };
        match event.kind() {
            EventKind::SessionForked if depth > 0 => {
                // What was said before the fork lives in the parent; carry it
                // first so this session's own turns follow it in order.
                let parent = event
                    .payload()
                    .get("parent_session_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|raw| raw.parse::<protocol::SessionId>().ok());
                let source_seq = event
                    .payload()
                    .get("source_seq")
                    .and_then(serde_json::Value::as_u64);
                if let (Some(parent), Some(source_seq)) = (parent, source_seq) {
                    // The parent's own latest compaction is only usable if
                    // it is not past the fork point; `None` means "scan".
                    let parent_compaction = block_on(client.get_session(parent), &kernel_cancel)
                        .ok()
                        .and_then(|snapshot| snapshot.last_compaction())
                        .filter(|seq| *seq <= source_seq);
                    conversation_through(
                        client,
                        parent,
                        Some((source_seq, parent_compaction)),
                        cancel,
                        depth - 1,
                        window,
                        history,
                    );
                }
            }
            EventKind::TurnStarted => {
                open = text_of(&event, "text");
            }
            EventKind::TurnCompleted => {
                if let Some(user) = open.take() {
                    history.push(
                        session_id,
                        event.seq(),
                        ConversationTurn::new(
                            user,
                            ConversationOutcome::Answered(text_of(&event, "text")),
                        ),
                    );
                }
            }
            EventKind::TurnFailed => {
                if let Some(user) = open.take() {
                    let reason = text_of(&event, "reason").unwrap_or_default();
                    history.push(
                        session_id,
                        event.seq(),
                        ConversationTurn::new(user, ConversationOutcome::Failed(reason)),
                    );
                }
            }
            EventKind::TurnInterrupted => {
                if let Some(user) = open.take() {
                    history.push(
                        session_id,
                        event.seq(),
                        ConversationTurn::new(user, ConversationOutcome::Interrupted),
                    );
                }
            }
            EventKind::ContextCompacted => fold(history, &event),
            _ => {}
        }
    }
}

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
    PreservedLiveContext::new(
        prompt,
        Vec::new(),
        agents_rules,
        String::new(),
        context_limit,
        output_reserve,
    )
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
    let permission_lattice = match exec_permission_lattice(
        workspace.as_ref().map(|(root, _)| root.as_path()),
        forced_mode,
    ) {
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
    let policy_version = apply_managed_ceilings(&mut tools);
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

    // Trusted-project integrations: web_fetch allowlist, hooks, shadow
    // diagnostics, MCP servers — before the model and the context, so a
    // `session_start` hook's output is visible to this run and MCP start-up
    // is not charged to the wall-time budget.
    if let Some((root, TrustStatus::Trusted)) = workspace.as_ref() {
        let mut warn = |line: &str| eprintln!("{line}");
        let session_hooks = configure_trusted_integrations(&mut tools, root, &mut warn);
        if !session_hooks.session_start.is_empty() {
            // Fire-and-forget: a session_start hook observes the run
            // starting, it never gates it (no PreHookOutcome here).
            let _ = crate::hooks::run_notify_hooks(
                &session_hooks.session_start,
                "session_start",
                serde_json::json!({}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
        // Fired by SessionEndHookGuard's Drop impl, on whatever exit path
        // this turn actually takes.
        session_end_guard.hooks = session_hooks.session_end;
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
    match load_active_reminders(workspace.as_ref().map(|(root, _)| root.as_path())) {
        Ok(Some((block, floor))) => {
            reminder_floor = floor;
            reminder_block = Some(block);
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("warning: reminders not loaded: {err}");
        }
    }

    // Layered model selection (env overrides > user config > typed fallback),
    // driven through the shared `resolve_model_plan`/`build_backing_model`
    // pair `rapid doctor` also runs — one interpretation of "which model
    // would this environment actually use", not two. Resolved before context
    // construction below — not after, as it was before this fix — so the
    // context budget (`context_budget_for`) is derived from the model that
    // will actually run this turn, never a hard-coded placeholder sized
    // before the model was even known.
    let process_env: Vec<(String, String)> = std::env::vars().collect();
    // Printed the moment each line is produced, exactly as when this block
    // was inline: the extraction must not reorder exec's stderr. Doctor
    // passes a collector into this same seam instead.
    let mut warn = |line: &str| eprintln!("{line}");
    let plan = match resolve_model_plan(&process_env, reminder_floor, &mut warn) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("{err}");
            return Ok(JsonlExitCode::Usage.as_i32());
        }
    };
    let base_url = plan
        .primary_config
        .as_ref()
        .map(|active| active.entry.base_url.clone())
        .unwrap_or_else(|| String::from("unconfigured"));
    let child_model_config = plan.primary_config;
    let models = plan.models;
    let unconfigured = plan.unconfigured;
    // One store per backend, fully built before any ConfiguredModel borrows
    // from it — every borrow below is a plain, compiler-checked immutable
    // borrow of an already-final Vec, not touched again afterward.
    let credential_stores: Vec<auth::InMemoryCredentialStore> = models
        .iter()
        .map(|_| auth::InMemoryCredentialStore::new())
        .collect();
    let diag = parsed.verbose.then(|| StepDiag::stderr(&base_url));
    let (backing, router_decisions) = match build_backing_model(
        &models,
        &credential_stores,
        unconfigured,
        diag,
        policy_version.clone(),
        &mut warn,
    ) {
        Ok(built) => built,
        Err(err) => {
            eprintln!("{err}");
            return Ok(JsonlExitCode::Usage.as_i32());
        }
    };

    // Prompt/context stack: project instructions (AGENTS.md convention +
    // compat paths) and the conditional-section system prompt (environment,
    // trust posture, token budget) — sized against `backing`, the model
    // just resolved above, never a fixed placeholder.
    let (context_limit, output_reserve) = context_budget_for(&backing);
    if parsed.verbose {
        // Provenance comes from `context_budget_source`, keyed off
        // `backing` itself (what actually produced the numbers above) —
        // shared with `rapid doctor`'s own context-budget check so both
        // describe the same resolution the same way.
        let source = context_budget_source(&backing, models.len());
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
        let retrieved = crate::context_retrieval::retrieve(root, &prompt, RETRIEVAL_BUDGET_TOKENS);
        preserved.with_retrieved_context(retrieved)
    } else {
        preserved
    };
    let preserved = preserved.with_reminders_block(reminder_block);
    // The run's own session in the project ledger. `exec_workspace` resolves
    // a project for any directory (the nearest marked ancestor, else the
    // directory itself — the TUI's rule), so a run is recorded wherever it
    // is run; `None` means the directory, home, or trust catalog could not
    // be resolved at all, and the run says so at the end rather than
    // leaving the user to look for a session that was never written.
    let recording = match (&workspace, &parsed.resume) {
        (Some((root, _)), ExecResume::Fresh) => match ExecRecording::open(root) {
            Ok(recording) => Some(recording),
            Err(reason) => {
                eprintln!("warning: this run is not being recorded: {reason}");
                None
            }
        },
        // A resume that cannot be honored is an error, not a fresh session
        // run in its place: the user asked for a conversation, and a turn
        // that silently forgot it would be the old behavior under a flag
        // that promises otherwise.
        (Some((root, _)), resume) => match ExecRecording::open_existing(root, resume) {
            Ok(recording) => Some(recording),
            Err(InteractiveError::UnknownSession(id)) => {
                eprintln!("rapid exec: session {id} has not been recorded in this project");
                let ledger_path = project_ledger_path(&root.join(PROJECT_MARKER));
                if let Some(hint) = known_sessions_hint(&ledger_path) {
                    eprint!("{hint}");
                }
                return Err(InteractiveError::Usage);
            }
            Err(InteractiveError::Usage) => {
                eprintln!(
                    "rapid exec: no session has been recorded in this project yet; \
run without --continue to start one"
                );
                return Err(InteractiveError::Usage);
            }
            Err(err) => return Err(err),
        },
        (None, ExecResume::Fresh) => None,
        (None, _) => {
            eprintln!(
                "rapid exec: --resume/--continue need a project; none could be resolved here"
            );
            return Err(InteractiveError::Usage);
        }
    };
    // The session's earlier turns, if this is one: the same history an
    // interactive turn carries, read the same way.
    let mut history_through = 0;
    let preserved = match &recording {
        Some(recording) if parsed.resume != ExecResume::Fresh => {
            let history_cancel = agent_runtime::CancellationToken::new();
            let history =
                conversation_history(&recording.client, recording.session_id, &history_cancel);
            history_through = history.through_seq;
            preserved
                .with_conversation(history.turns)
                .with_compaction_summary(history.summary)
        }
        _ => preserved,
    };
    let turn_text = prompt.clone();
    let spec = AgentSpec::builder(
        protocol::AgentId::new(),
        AgentRole::Coder,
        prompt,
        protocol::WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    .map_err(|_| InteractiveError::Internal)?;
    let session_id = recording
        .as_ref()
        .map(|recording| recording.session_id)
        .unwrap_or_else(protocol::SessionId::new);
    let request = AgentExecutionRequest::new(spec, session_id);
    let cancel = agent_runtime::CancellationToken::new();
    if let Some(max_wall_time) = parsed.max_wall_time {
        spawn_wall_time_watchdog(cancel.clone(), max_wall_time);
    }
    let mut events: Vec<agent_runtime::TurnEvent> = Vec::new();
    // The turn's start goes through the kernel's `SubmitTurn` exactly as an
    // interactive turn's does — that is what carries the prompt into the
    // ledger — and the tools get the same ledger-backed observers, so what
    // this run writes and runs reaches `/diff` and `/jobs` on resume.
    let recorded_turn = match &recording {
        Some(recording) => match recording.start_turn(&turn_text) {
            Ok(turn_id) => {
                attach_ledger_sinks(&mut tools, &recording.client, session_id, &recording.actor);
                Some(turn_id)
            }
            Err(reason) => {
                eprintln!("warning: this run is not being recorded: {reason}");
                None
            }
        },
        None => None,
    };
    let mut sink = RecordedEvents {
        events: &mut events,
        ledger: recorded_turn
            .and(recording.as_ref())
            .map(ExecRecording::sink),
    };

    if let Some((root, TrustStatus::Trusted)) = workspace.as_ref() {
        // The ledger sinks were attached with the recorded turn above;
        // `None` here leaves them as they are.
        configure_trusted_model_tools(
            &mut tools,
            root,
            child_model_config.as_ref(),
            &permission_lattice,
            None,
        );
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
    let goal_path = workspace
        .as_ref()
        .map(|(root, _)| root.join(PROJECT_MARKER).join(GOAL_FILE));
    let goal_id = goal_path.as_deref().and_then(active_goal_id);
    let turn_started = Instant::now();
    let run_result = if let Some(schema_path) = parsed.json_schema.as_ref() {
        let schema_text = match std::fs::read_to_string(schema_path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!(
                    "--json-schema: failed to read {}: {err}",
                    schema_path.display()
                );
                return Ok(JsonlExitCode::Usage.as_i32());
            }
        };
        let schema_value: serde_json::Value = match serde_json::from_str(&schema_text) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "--json-schema: {} is not valid JSON: {err}",
                    schema_path.display()
                );
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
            &mut sink,
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
            &mut sink,
            &cancel,
            ContextRetryPolicy::default(),
            diag,
        )
    };
    if let (Some(recording), Some(turn_id)) = (&recording, recorded_turn) {
        recording.finish_turn(turn_id, &run_result, history_through);
    }
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
        if let Some(io) = jsonl_io.as_mut()
            && let Ok(record) = crate::headless::jsonl::JsonlRecord::router_decision(
                session_id,
                next_jsonl_seq,
                crate::headless::jsonl::now_rfc3339(),
                &decision.requested_model,
                resolved,
                reason_tag,
                decision.spent_usd_micros,
                decision.policy_version.as_deref(),
            )
        {
            let _ = io.records().write(&record);
            next_jsonl_seq += 1;
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
                (
                    Some(question),
                    JsonlExitCode::NeedsContext,
                    outcome.cost_usd_micros,
                )
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
    // Where the record went, on stderr so stdout stays the answer. A run that
    // was recorded names the session — the id is otherwise only discoverable
    // by listing sessions and guessing — and a run outside a project says
    // why there is none, rather than leaving the user to look for it.
    match (&recording, recorded_turn, &workspace) {
        (Some(recording), Some(_), _) if parsed.resume != ExecResume::Fresh => eprintln!(
            "session {} continued; `rapid exec --continue` runs its next turn, `rapid resume {}` \
reopens it",
            recording.session_id, recording.session_id
        ),
        (Some(recording), Some(_), _) => eprintln!(
            "session {} recorded in this project; `rapid exec --continue` runs its next turn, \
`rapid resume {}` reopens it",
            recording.session_id, recording.session_id
        ),
        // `exec_workspace` falls back to the current directory as its own
        // project root, exactly as the TUI does, so this is not "outside a
        // project" — it is a directory, home, or trust catalog that could
        // not be resolved at all, and the tools were withheld for the same
        // reason (see the warning printed above).
        (_, _, None) => eprintln!(
            "not recorded: the project root, home directory, or trust catalog could not be \
resolved, so there is nowhere to record to"
        ),
        // The ledger could not be opened or the turn not started: the
        // warning was printed where it happened.
        _ => {}
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
    // Resuming subscribes from sequence 0, which the kernel replays before
    // tailing, so the transcript is rebuilt from the durable event ledger.
    // A fresh session subscribes from its own tip: there is no history to
    // replay, and asking for one would re-deliver its `SessionCreated`.
    let (session_id, mut ui, from_seq, replay_through) = match options.resume {
        Some(resume) => {
            let snapshot = block_on(client.get_session(resume), &options.cancel).map_err(
                |err| match &err {
                    // The common case by far: a typo, or an id from another
                    // project (the ledger is per-project, so a real id from
                    // elsewhere is simply absent here). Every other kernel
                    // failure keeps its own diagnosis.
                    InteractiveError::Kernel(api)
                        if api.code() == protocol::ErrorCode::SessionNotFound =>
                    {
                        InteractiveError::UnknownSession(resume)
                    }
                    _ => err,
                },
            )?;
            // Deliberately *not* seeded with the snapshot: `reduce`'s kernel
            // path requires each event's seq to be exactly `snapshot.seq +
            // 1`, so seeding with the current tip would make every replayed
            // event out of order and discard the whole history — the
            // transcript would come back empty. The replay rebuilds the
            // projection from `SessionCreated` forward, and
            // `drain_kernel_events`' own snapshot refresh reconciles once
            // it has caught up.
            // `snapshot.seq()` is the tip to replay *through*. Anything
            // committed after this read is not history but live tail, and
            // arrives through the ordinary drain.
            (snapshot.id(), AppState::new(), 0, snapshot.seq())
        }
        None => {
            let snapshot = block_on(
                client.create_session(CreateSession::new(
                    ProjectId::new(),
                    actor.clone(),
                    TraceId::new(),
                )),
                &options.cancel,
            )?;
            let seq = snapshot.seq();
            let id = snapshot.id();
            (
                id,
                reduce(AppState::new(), &UiEvent::Snapshot(snapshot)),
                seq,
                seq,
            )
        }
    };
    let mut stream = block_on(
        client.subscribe(SubscribeEvents::new(session_id, from_seq)),
        &options.cancel,
    )?;
    if options.resume.is_some() {
        // Fold the replayed history before the first paint, so a resumed
        // session shows its transcript immediately instead of filling in
        // over the next few ticks — what was said before a fork included.
        inherit_transcript(&client, &mut ui, session_id, &options.cancel);
        replay_history(
            &client,
            &mut stream,
            &mut ui,
            session_id,
            replay_through,
            &options.cancel,
        )?;
    }
    // Project the persisted composition-root goal into the interactive TUI so
    // the Goals route shows it (the TUI is a projection of runtime state).
    sync_persisted_goal(&mut ui, &resolved.ledger_path);
    sync_configured_models(&mut ui);
    sync_memory_index(&mut ui, &resolved.root);
    // Anything the ledger unification had to say goes in the transcript: it
    // is addressed to the user, and stderr written before the alt screen
    // opens is wiped before it can be read.
    if let Some(notice) = resolved.ledger_notice.clone() {
        ui = reduce(
            ui,
            &UiEvent::Local(LocalUiEvent::AppendCommandOutput(notice)),
        );
    }
    let mut interrupt_count = 0;
    let mut saw_ctrl_c = false;

    // A trusted project's session hooks, on the same per-run terms as a
    // headless run's: `session_start` now — before the alt screen, so a
    // slow hook is not a blank screen — and `session_end` on every exit
    // path (the guard's `Drop`), neither gating anything.
    let mut session_end_guard = SessionEndHookGuard::default();
    if resolved.trust.is_trusted() {
        let hooks = load_project_integrations(&resolved.root).hooks;
        if !hooks.session_start.is_empty() {
            let _ = crate::hooks::run_notify_hooks(
                &hooks.session_start,
                "session_start",
                serde_json::json!({}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
        session_end_guard.hooks = hooks.session_end;
    }
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
    // The status bar's session-level facts, resolved through the same call
    // the turn itself makes, so the bar shows the mode that will actually
    // govern tool calls rather than a second guess at it. A resolution
    // failure leaves the dash: the bar never asserts a posture it could not
    // confirm.
    if let Ok(lattice) = exec_permission_lattice(Some(&resolved.root), None) {
        renderer.chrome = session_status_chrome(lattice.mode());
    }
    let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // One job table for the whole session: background jobs outlive the turn
    // that started them, and dropping this at the end of `run_started_session`
    // is what kills them. See `SessionLoop::jobs`.
    let session_jobs = crate::exec_tools::JobRegistry::default();
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
        user_home: &resolved.user_home,
        trusted: resolved.trust.is_trusted(),
        turn_in_flight: turn_in_flight.clone(),
        jobs: session_jobs.clone(),
        renderer: &mut renderer,
        autonomous: None,
        compaction: None,
        shared: SessionShared::default(),
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
    /// The session's own resolved RapidLM home — see
    /// [`ResolvedProject::user_home`]. Slash commands that consult project
    /// trust must read the catalog *this* session read.
    user_home: &'a Path,
    trusted: bool,
    /// Set while a turn spawned by `submit_turn` is executing on its own
    /// thread; a new plain-text submission is a no-op while this is set,
    /// rather than reaching `kernel::SubmitTurn` and hitting the exact
    /// `SessionConflict` this whole feature exists to stop crashing on.
    turn_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The session's background-job table, shared into every turn.
    ///
    /// Owned here rather than per turn because a background job must outlive
    /// the turn that started it — `shell_exec background=true` tells the
    /// model to poll it, and a user typing `/jobs` does so between turns,
    /// when a per-turn table would already have been dropped (and its
    /// children killed). Dropping this at the end of the session is what
    /// still guarantees no command outlives the CLI.
    jobs: crate::exec_tools::JobRegistry,
    renderer: &'a mut TuiRenderer,
    /// `Some` for the entire duration of a `/goal run`-started autonomous
    /// continuation, `None` otherwise. Owned by the loop, not a reference:
    /// this state changes (starts/stops) across the loop's own lifetime the
    /// same way `turn_in_flight` does, but — unlike `turn_in_flight` — needs
    /// no sharing with a spawned thread, since only the main loop itself
    /// ever reads or decides from it (see `SessionLoop::step_autonomous_goal`).
    autonomous: Option<AutonomousGoalState>,
    /// `Some` while a `/compact` runs on its own thread — see
    /// [`SessionLoop::compact_session`]. Holds the turn slot the way
    /// `turn_in_flight` does (one model call per session at a time) and is
    /// cleared by `settle_compaction` once the thread reports back.
    compaction: Option<CompactionInFlight>,
    /// What the turn and compaction threads share with the loop — the
    /// notices they leave for the user (drained into the transcript on every
    /// tick) and the session's MCP connections. See [`SessionShared`].
    shared: SessionShared,
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

/// Warnings a turn or compaction thread has for the user — a model config
/// warning, an MCP server that would not start, a reminder roster that did
/// not load. Headless prints these to stderr; under the TUI's alt screen a
/// raw stderr write only corrupts the frame, so the threads leave them
/// here and the loop puts them in the transcript on its next tick.
type SessionNotices = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// What every turn and compaction thread of a session shares with the
/// loop and with each other: the notices channel and the session's MCP
/// connections (spawned once, reused by every turn — see
/// [`crate::exec_tools::McpRegistry`]). Cloned into each thread; the
/// clones are handles.
#[derive(Clone, Default)]
struct SessionShared {
    notices: SessionNotices,
    mcp: crate::exec_tools::McpRegistry,
    /// The session's running subagents — what `/agents cancel` acts on.
    agents: crate::exec_tools::SubagentRegistry,
    /// Test-only seam: a subagent runner for scripted turns, which have no
    /// configured model to build the real one from.
    #[cfg(test)]
    scripted_subagents: Option<std::sync::Arc<dyn crate::exec_tools::SubagentRunner>>,
}

/// Leave `line` for the loop to show. Bounded: a turn that has a lot to
/// say keeps its first lines, not an unbounded backlog.
fn notify(notices: &SessionNotices, line: &str) {
    const MAX_PENDING_NOTICES: usize = 64;
    let mut pending = notices.lock().unwrap_or_else(|p| p.into_inner());
    if pending.len() < MAX_PENDING_NOTICES {
        pending.push(line.to_owned());
    }
}

/// A `/compact` running on its own thread — see
/// [`SessionLoop::compact_session`].
struct CompactionInFlight {
    /// Cancels the model call; Ctrl-C sets it.
    cancel: agent_runtime::CancellationToken,
    /// Written exactly once, by the thread, when it finishes.
    outcome: CompactionOutcomeSlot,
}

/// Where a compaction thread leaves its result for the loop to collect.
type CompactionOutcomeSlot =
    std::sync::Arc<std::sync::Mutex<Option<Result<CompactionReport, String>>>>;

/// What a finished compaction reports back: the summary itself reaches the
/// loop through the ledger, as `context.compacted`.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompactionReport {
    Compacted {
        turns: usize,
        tokens: u64,
    },
    /// No completed turns to fold — since the last compaction, when there
    /// was one.
    NothingToCompact {
        compacted_before: bool,
    },
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
        let result = self.run_until_quit(inputs);
        // A compaction still out when the session ends is not a turn, so
        // the exit path's kernel interrupt cannot reach it; its own token
        // can, and its thread stops at the next check instead of holding
        // a model call open until the process is gone.
        if let Some(compaction) = &self.compaction {
            compaction.cancel.cancel();
        }
        result
    }

    fn run_until_quit(
        &mut self,
        inputs: &mut InputSource,
    ) -> Result<InteractiveOutcome, InteractiveError> {
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
                // A running compaction is not a turn, so the kernel
                // interrupt below cannot reach it; its own token can.
                if let Some(compaction) = &self.compaction {
                    compaction.cancel.cancel();
                }
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
        // Parsed against the session so the short ids the panels display
        // (`job-3`, an id's random tail) resolve to the typed ids the
        // commands take. A bare `parse_command` accepts full UUIDs only.
        match parse_command_in(command, self.ui) {
            Ok(parsed) => match dispatch(parsed) {
                FrontendAction::Quit => Ok(LoopControl::Quit(InteractiveOutcome::Quit)),
                FrontendAction::Local(LocalAction::Open(inspector)) => {
                    match inspector.route() {
                        Some(route) => {
                            *self.ui = reduce(
                                self.ui.clone(),
                                &UiEvent::Local(LocalUiEvent::SetRoute(route)),
                            );
                            self.focus_inspector(&inspector);
                        }
                        // Eleven of the seventeen inspectors have no TUI
                        // route. This arm used to be an empty `if let`, so
                        // `/mcp list`, `/sandbox doctor`, `/model list`,
                        // `/plugins list`, `/policy explain`, `/knowledge
                        // list`, `/playbook list`, `/trace show`, `/insights
                        // show`, `/permissions` and `/computer status` each
                        // parsed successfully, dispatched successfully, and
                        // then did *nothing at all* — no panel, no output,
                        // no error. That is a worse failure than the
                        // `KernelAction` path next to it, which has named
                        // its specific gap since it existed.
                        None => self.open_unrouted_inspector(inspector),
                    }
                    Ok(LoopControl::Continue)
                }
                FrontendAction::Local(LocalAction::Permissions(intent)) => {
                    self.apply_permissions_intent(intent);
                    Ok(LoopControl::Continue)
                }
                FrontendAction::InlineHelp(help) => {
                    // Bare `/help` listed 28 command families with nothing
                    // to say that roughly half report "not available" the
                    // moment they run. `/help <command>` keeps its own
                    // usage text unchanged.
                    let text = match help.topic() {
                        None => crate::command_help::annotated_catalog_help(),
                        Some(_) => help.usage().to_owned(),
                    };
                    self.append_command_output(text);
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

    /// Apply the selection an inspector command named, once its route is set.
    ///
    /// [`Inspector`] has carried a selection since `/diff --agent` existed,
    /// but [`Inspector::route`] — the only thing that reached the panel —
    /// returns a bare [`tui::state::UiRoute`] and dropped it. So `/jobs show
    /// <id>` and `/jobs logs <id>` parsed an id, dispatched successfully,
    /// and opened the same unfiltered list as a bare `/jobs`.
    ///
    /// The selection is applied in the same dispatch as the route change so
    /// the panel can never be showing one job's route with another job's
    /// selection, and it reuses `AppState`'s existing `selected_job` rather
    /// than adding a second place to record which job is in view.
    fn focus_inspector(&mut self, inspector: &Inspector) {
        let (id, logs) = match inspector {
            // `/agents show <id>`: the panel resolves `selected_agent` into
            // its selected row and paints that row's detail block, so this
            // is the whole fix — the field simply had no writer.
            Inspector::Agents { id } => {
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SelectAgent(*id)),
                );
                return;
            }
            Inspector::Jobs { id, logs } => (id, logs),
            Inspector::Context { query } => {
                let found = query.as_deref().map(|query| self.search_context(query));
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SyncContextSearch(found)),
                );
                return;
            }
            Inspector::Diff { agent } => {
                // The one operand in this family that cannot be honored.
                // `workspace.mutation_detected` carries `path`/`lines_before`
                // /`lines_after` and no agent, and subagents write through
                // the *parent* turn's sink, so nothing in the projection
                // could tell one agent's writes from another's. Attributing
                // them means recording an agent at the write site — a
                // feature, not wiring. Until then the flag says so rather
                // than quietly showing every change as though it had
                // filtered them.
                *self.ui = reduce(
                    self.ui.clone(),
                    &UiEvent::Local(LocalUiEvent::SelectDiffAgent(*agent)),
                );
                return;
            }
            // Every other inspector names no entity to focus.
            _ => return,
        };
        let target = match (id, logs) {
            (Some(id), _) => Some(*id),
            // A bare `/jobs logs` means the newest job. Without this the
            // command is answerable only by someone who already knows a
            // `JobId`, and nothing shows one: the panel paints a job's
            // *command* whenever the producer recorded one, precisely
            // because a list of UUIDs cannot tell a reader which row is the
            // test run they are waiting on. `JobId` is a UUIDv7, so the
            // projection's key order is start order and the last key is the
            // most recently started job.
            (None, true) => self.ui.jobs().keys().next_back().copied(),
            (None, false) => None,
        };
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::SelectJob(target)),
        );
        // A page belongs to exactly one job: `/jobs` and `/jobs show` must
        // clear whatever `/jobs logs` last painted rather than leave it
        // under a different selection.
        let page = match (logs, target) {
            (true, Some(id)) => Some(self.job_log_page(id)),
            _ => None,
        };
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::SyncJobLogs(page)),
        );
    }

    /// Run proactive retrieval for `query` and project what it found.
    ///
    /// The same `context_retrieval::retrieve` call the turn path makes, so
    /// this answers "what would the agent be given if I asked this" rather
    /// than describing some separate index.
    ///
    /// **Gated on project trust exactly as the turn path is.** Retrieval
    /// walks the tree and writes an incremental index under
    /// `.rapidlm/index/`; doing that for an untrusted project because
    /// someone typed a slash command would be a trust boundary crossed by
    /// the UI, so an untrusted project reports that instead of searching.
    fn search_context(&mut self, query: &str) -> tui::state::ContextSearchView {
        use tui::state::{ContextHit, ContextSearchOutcome, ContextSearchView};

        if !self.trusted {
            return ContextSearchView::new(
                query.to_owned(),
                Vec::new(),
                ContextSearchOutcome::Untrusted,
            );
        }
        // A first index of a large repo can take up to
        // `context_retrieval::RETRIEVAL_TIMEOUT`, and this runs on the input
        // thread. Paint why the session paused before blocking on it, or a
        // slow repo looks like a hang.
        self.append_command_output(format!("searching retrieved context for {query}"));
        let _ = self.renderer.render(self.ui);
        let hits = crate::context_retrieval::retrieve(self.root, query, RETRIEVAL_BUDGET_TOKENS)
            .into_iter()
            .map(|block| ContextHit {
                locator: block.locator().to_owned(),
                bytes: block.text().len() as u64,
            })
            .collect();
        ContextSearchView::new(query.to_owned(), hits, ContextSearchOutcome::Searched)
    }

    /// The named job's captured output, as a page for the `/jobs logs` view.
    ///
    /// Empty when the job is unknown to this process's job table — a job
    /// started by an *earlier* process is in the ledger projection (so the
    /// panel can still name it) while its spooled bytes died with the
    /// process that captured them. The panel says so rather than implying
    /// the job printed nothing.
    fn job_log_page(&self, id: protocol::JobId) -> tui::state::JobLogView {
        match self.jobs.logs(id) {
            Some((text, truncated)) => {
                let lines = text.lines().map(str::to_owned).collect();
                tui::state::JobLogView::new(id, lines, truncated)
            }
            None => tui::state::JobLogView::new(id, Vec::new(), false),
        }
    }

    /// The project and RapidLM home *this session* resolved, handed to
    /// `rapid mcp`'s own entry point.
    ///
    /// Deliberately not `std::env::vars()`: a session started with an
    /// explicit `user_home` (every test, and any embedder) would otherwise
    /// have `/mcp list` report trust from `$HOME/.rapidlm` — a catalog the
    /// session itself never read.
    fn mcp_env(&self) -> crate::mcp_admin::McpEnv {
        crate::mcp_admin::McpEnv {
            cwd: self.root.to_path_buf(),
            env: Vec::new(),
            home: Some(self.user_home.to_path_buf()),
        }
    }

    /// Tools this session already saw denied, so `/permissions` can name
    /// candidates rather than making a user reconstruct them from the
    /// transcript.
    ///
    /// Read from the transcript the production event fold built, which is
    /// the only record of it the frontend has: `TurnEvent::ToolDenied`
    /// carries the tool name but *not* the reason, so this deliberately does
    /// not claim a grant would help — a call denied by a deny rule, plan
    /// mode, or a managed-policy ban stays denied whatever is granted.
    fn denied_this_session(&self) -> String {
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for entry in self.ui.transcript() {
            if let tui::state::TranscriptEntry::ToolActivity { tool, status, .. } = entry
                && *status == tui::state::ToolActivityStatus::Denied
            {
                seen.insert(tool.as_str());
            }
        }
        if seen.is_empty() {
            return String::new();
        }
        format!(
            "denied-this-session={}\nnote: `/permissions allow <tool>` pre-approves a call \
that was denied for approval; one denied by a rule, plan mode, or managed policy stays \
denied\n",
            seen.into_iter().collect::<Vec<_>>().join(",")
        )
    }

    /// `/permissions allow|revoke <pattern>` through the production
    /// `rapid permissions` path.
    ///
    /// The same `permissions_cli::run` the subcommand uses, with this
    /// session's own project and RapidLM home — no second writer, and no
    /// second interpretation of the pattern grammar or the store format.
    ///
    /// Not approval-gated, unlike `/mcp remove`: a grant *is* the user's
    /// approval, so gating it on an approval would be circular. It is also
    /// the only way, in this build, for a user to act on the denial the
    /// default permission mode produces — see `newtask.md`'s open decision.
    fn apply_permissions_intent(&mut self, intent: PermissionsIntent) {
        let (verb, pattern) = match intent {
            PermissionsIntent::Allow { pattern } => ("allow", pattern),
            PermissionsIntent::Revoke { pattern } => ("revoke", pattern),
        };
        let outcome =
            crate::permissions_cli::run(&[verb.to_owned(), pattern], &self.permissions_env());
        match outcome {
            Ok(outcome) if outcome.exit == 0 => self.append_command_output(outcome.text),
            Ok(outcome) => self.append_command_error(outcome.text),
            Err(crate::permissions_cli::PermissionsUsageError(message)) => {
                self.append_command_error(message)
            }
        }
    }

    /// The project and RapidLM home *this session* resolved, for
    /// `rapid permissions`' own entry point — the same reasoning as
    /// [`Self::mcp_env`].
    fn permissions_env(&self) -> crate::permissions_cli::PermissionsEnv {
        crate::permissions_cli::PermissionsEnv {
            cwd: self.root.to_path_buf(),
            env: Vec::new(),
            home: Some(self.user_home.to_path_buf()),
        }
    }

    /// An inspector the TUI has no route for: render whatever real report
    /// this build can produce, or say precisely why it cannot.
    ///
    /// Never silent. Where a headless command already answers the same
    /// question, the message names it — those are verified to exist in
    /// `run_subcommand`'s dispatch table by
    /// `every_command_an_unrouted_inspector_message_names_actually_exists`.
    fn open_unrouted_inspector(&mut self, inspector: Inspector) {
        if matches!(inspector, Inspector::Permissions) {
            // The real report `rapid permissions list` prints, from the same
            // store a real run reads — not a note saying there is no panel.
            match crate::permissions_cli::run(&["list".to_owned()], &self.permissions_env()) {
                Ok(outcome) => {
                    let mut text = outcome.text;
                    text.push_str(&self.denied_this_session());
                    self.append_command_output(text);
                }
                Err(crate::permissions_cli::PermissionsUsageError(message)) => {
                    self.append_command_error(message);
                }
            }
            return;
        }
        if matches!(inspector, Inspector::Mcp) {
            // `/mcp list` and `/mcp doctor` both land here. The report is
            // the one `rapid mcp list` prints, from the same loader the
            // turn path registers from — not a second view of the same
            // settings files.
            let outcome = crate::mcp_admin::run(&["list".to_owned()], &self.mcp_env());
            match outcome {
                Ok(outcome) => {
                    let mut text = outcome.text;
                    // Deliberately not probing from inside the event loop:
                    // a server that never answers costs 30 seconds each,
                    // which would freeze the session.
                    text.push_str(
                        "run `rapid mcp probe` for a live handshake against each server \
(it starts them, so it needs a trusted project)\n",
                    );
                    self.append_command_output(text);
                }
                Err(crate::mcp_admin::McpUsageError(message)) => {
                    self.append_command_error(message);
                }
            }
            return;
        }
        self.append_command_error(unrouted_inspector_text(&inspector));
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
        if self.model_busy() {
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

    /// Move this live session onto `target`, rebuilding its transcript from
    /// the durable ledger.
    ///
    /// The same three steps `rapid resume` performs, for the same reasons:
    /// read the tip, subscribe from 0 so the kernel replays, and fold into a
    /// *fresh* `AppState` — seeding one with the tip would make every
    /// replayed event violate `apply_next`'s seq-continuity rule and discard
    /// the whole history. Replacing `*self.stream` drops the old
    /// subscription, which stops its worker thread.
    ///
    /// Host-owned chrome (the configured models, the memory index) is
    /// re-synced rather than carried across, because it is read from files
    /// and environment and belongs to the *project*, not to either session —
    /// re-reading it is both simpler and correct if it changed.
    fn switch_to_session(&mut self, target: protocol::SessionId) -> Result<(), InteractiveError> {
        let snapshot = block_on(self.client.get_session(target), self.cancel)?;
        let mut fresh = AppState::new();
        let mut stream = block_on(
            self.client.subscribe(SubscribeEvents::new(target, 0)),
            self.cancel,
        )?;
        // A fork's own ledger starts at `session.forked`: what was said
        // before it is the parent's, and a rewind that showed an empty
        // transcript looked like a session with no past rather than one
        // rewound to a point in it.
        inherit_transcript(self.client, &mut fresh, target, self.cancel);
        replay_history(
            self.client,
            &mut stream,
            &mut fresh,
            target,
            snapshot.seq(),
            self.cancel,
        )?;
        // `sync_persisted_goal` takes the ledger path and reads the goal
        // beside it, so hand it the project's resolved ledger.
        sync_persisted_goal(
            &mut fresh,
            &project_ledger_path(&self.root.join(PROJECT_MARKER)),
        );
        sync_configured_models(&mut fresh);
        sync_memory_index(&mut fresh, self.root);
        *self.stream = stream;
        *self.ui = fresh;
        self.session_id = target;
        Ok(())
    }

    /// `/jobs cancel [id]`: stop one background job, or every running one.
    ///
    /// Reports what actually happened rather than acknowledging blindly — a
    /// handle that is not in the table, or one whose job already finished,
    /// are different answers from "stopped it", and a user who is told
    /// "cancelled" believes the command is no longer running.
    /// Refuse a session switch while a turn or an autonomous goal owns the
    /// session, and say why. Returns `true` (after painting) if refused.
    ///
    /// One guard for `/fork`, `/resume` and `/rewind`: a switch mid-turn
    /// would branch from a sequence the turn is still writing to and move
    /// the session out from under a thread still emitting into it. Three
    /// hand-copied versions of this check is how one of them drifts.
    fn refuse_if_busy(&mut self, before: &str) -> Result<bool, InteractiveError> {
        if self
            .turn_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.append_command_error(format!(
                "a turn is running; wait for it to finish before {before}"
            ));
            self.drain()?;
            return Ok(true);
        }
        if self.compaction.is_some() {
            self.append_command_error(format!(
                "a compaction is running; wait for it to finish before {before}"
            ));
            self.drain()?;
            return Ok(true);
        }
        if self.autonomous.is_some() {
            self.append_command_error(format!(
                "an autonomous goal is running; stop it with /goal stop before {before}"
            ));
            self.drain()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// `/resume [session]`: move this TUI onto another session of this
    /// project.
    ///
    /// This was refused as "cross-process session resume is not wired yet"
    /// while `switch_to_session` — the exact mechanism, built so `/fork`
    /// could move onto its child — sat beside it; the fork message even
    /// told the user to leave the TUI and run `rapid resume` to get back.
    /// Same guards as `/fork`, for the same reason: the switch drops the
    /// stream a running turn is still emitting into.
    ///
    /// Bare `/resume` from inside a session means "the other one": the most
    /// recently active session that is not this one, resolved by the same
    /// `recorded_sessions`/`newest_usable` pair `rapid resume` uses, so the
    /// two cannot disagree about what this project holds. An id the ledger
    /// has never recorded is refused *before* switching, with the same hint
    /// the CLI prints — `switch_to_session` would otherwise fail generically
    /// after tearing down the current subscription.
    fn resume_session(
        &mut self,
        session: Option<protocol::SessionId>,
    ) -> Result<(), InteractiveError> {
        if self.refuse_if_busy("resuming another session")? {
            return Ok(());
        }
        let ledger_path = project_ledger_path(&self.root.join(PROJECT_MARKER));
        let sessions = recorded_sessions(&ledger_path)?;
        let current = self.session_id.to_string();
        let target = match session {
            Some(id) => id,
            None => {
                let others: Vec<_> = sessions
                    .iter()
                    .filter(|summary| summary.session_id != current)
                    .cloned()
                    .collect();
                match newest_usable(others) {
                    Some(id) => id,
                    None => {
                        self.append_command_error(
                            "no other session is recorded in this project".to_owned(),
                        );
                        return self.drain();
                    }
                }
            }
        };
        if target == self.session_id {
            self.append_command_output(format!("already on session {target}"));
            return self.drain();
        }
        if !sessions
            .iter()
            .any(|summary| summary.session_id == target.to_string())
        {
            let mut text = format!("no session {target} in this project\n");
            if let Some(hint) = hint_lines(sessions) {
                text.push_str(&hint);
            }
            self.append_command_error(text);
            return self.drain();
        }
        let previous = self.session_id;
        self.switch_to_session(target)?;
        self.append_command_output(format!(
            "now on session {target}\n`/resume {previous}` returns to the one you left\n"
        ));
        self.drain()
    }

    /// `/agents cancel [id]` and `/agents terminate [id]`: stop one running
    /// subagent, or every running one. A subagent runs inside its parent
    /// turn's `task_spawn` call, so stopping it ends that call with a
    /// cancelled report and the parent turn goes on — the turn itself is
    /// Ctrl-C's to stop. Terminate is cancel: an in-process child has no
    /// harsher stop than its token.
    fn cancel_agent(&mut self, id: Option<protocol::AgentId>) -> Result<(), InteractiveError> {
        let text = match id {
            Some(id) if self.shared.agents.cancel(id) => format!("cancelled agent {id}"),
            Some(id) => format!("no running agent {id}"),
            None => match self.shared.agents.cancel_all() {
                0 => "no running agents to cancel".to_owned(),
                1 => "cancelled 1 running agent".to_owned(),
                n => format!("cancelled {n} running agents"),
            },
        };
        self.append_command_output(text);
        self.drain()
    }

    fn cancel_job(&mut self, id: Option<protocol::JobId>) -> Result<(), InteractiveError> {
        let text = match (id, self.jobs.cancel(id)) {
            (Some(id), None) => format!("no job {id} in this session"),
            (Some(id), Some(0)) => format!("job {id} had already finished"),
            (Some(id), Some(_)) => format!("cancelled job {id}"),
            (None, Some(0) | None) => "no running jobs to cancel".to_owned(),
            (None, Some(1)) => "cancelled 1 running job".to_owned(),
            (None, Some(n)) => format!("cancelled {n} running jobs"),
        };
        self.append_command_output(text);
        self.drain()
    }

    fn apply_kernel_action(&mut self, action: KernelAction) -> Result<(), InteractiveError> {
        self.cancel
            .check()
            .map_err(|_| InteractiveError::Cancelled)?;
        // One gate, consulted here *and* by `/help`'s availability
        // annotation, so what the listing claims and what this function does
        // cannot disagree. Previously the two were hand-mirrored: deleting an
        // arm below would have left `/help` still reporting the command as
        // working, with no compile error.
        if !crate::command_help::kernel_action_is_supported(&action) {
            self.append_command_error(unsupported_command_text(&action));
            return self.drain();
        }
        match action {
            KernelAction::StartGoal { statement } => self.start_goal(statement)?,
            KernelAction::PauseGoal => self.goal_lifecycle_command(GoalLifecycleKind::Pause)?,
            KernelAction::ResumeGoal => self.goal_lifecycle_command(GoalLifecycleKind::Resume)?,
            KernelAction::CancelGoal => self.goal_lifecycle_command(GoalLifecycleKind::Cancel)?,
            KernelAction::CancelJob { id } => self.cancel_job(id)?,
            KernelAction::CancelAgent { id } | KernelAction::TerminateAgent { id } => {
                self.cancel_agent(id)?;
            }
            KernelAction::ResumeSession { session } => self.resume_session(session)?,
            KernelAction::CompactSession => self.compact_session()?,
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
                    // `CancelJob`/`CancelAgent`/`TerminateAgent` all map to
                    // `KernelApi::Interrupt`, whose only implementation is a
                    // *session-wide* interrupt that takes no id — naming one
                    // used to kill the current turn instead of the thing
                    // named. Each of those has its own arm above now (the
                    // session's job table, its subagent registry), so
                    // anything reaching here is a genuine session-wide
                    // interrupt.
                    self.interrupt()?;
                }
                KernelApi::SubmitTurn => {
                    self.submit_turn("")?;
                }
                KernelApi::ForkSession => {
                    if self.refuse_if_busy("forking")? {
                        return Ok(());
                    }
                    // The ledger's tip, for the same reason `submit_turn`
                    // uses it: forking at the lagging projection's seq
                    // would branch from *before* the last turn's terminal
                    // events, silently leaving them out of the child.
                    let seq = self.session_tip()?;
                    let child = block_on(
                        self.client.fork_session(ForkSession::new(
                            self.session_id,
                            seq,
                            self.actor.clone(),
                            TraceId::new(),
                        )),
                        self.cancel,
                    )?;
                    let child_id = child.id();
                    let parent_id = self.session_id;
                    // Forking and then staying on the parent is not what a
                    // user means by it: the branch exists to be worked in.
                    // This used to reduce the child snapshot into the UI and
                    // say nothing, which *looked* like a switch for one frame
                    // and then silently reverted, because the id and the
                    // subscribed stream both still pointed at the parent.
                    self.switch_to_session(child_id)?;
                    self.append_command_output(format!(
                        "forked at seq {seq}: now on child session {child_id}\n\
the parent is unchanged; `/resume {parent_id}` returns to it\n"
                    ));
                }
                KernelApi::Rewind => {
                    let to_seq = match &other {
                        KernelAction::RewindSession { to_seq } => *to_seq,
                        _ => None,
                    };
                    // Unreachable now that `parse_rewind` requires the
                    // sequence — kept as a local error rather than the
                    // previous bare `return Ok(())`, which did nothing,
                    // printed nothing, and skipped the trailing `drain()`
                    // so the frame was not even repainted.
                    let Some(to_seq) = to_seq else {
                        self.append_command_error(
                            "rewind needs a sequence number: /rewind <seq>".to_owned(),
                        );
                        return self.drain();
                    };
                    if self.refuse_if_busy("rewinding")? {
                        return Ok(());
                    }
                    // A rewind is a fork at the sequence, then the same
                    // switch `/fork` makes onto its child. The previous
                    // version asked the kernel for a *prefix projection*
                    // (`KernelApi::Rewind`, which replays 1..seq and mutates
                    // nothing) and swapped that into the UI — leaving the
                    // ledger and the live subscription at the tip. Every
                    // `submit_turn` after that carried the projection's seq
                    // against the kernel's real tip and failed with
                    // `SessionConflict`: a "supported" command after which the
                    // session could not accept input. Forking is what the
                    // append-only ledger allows (history is never truncated),
                    // and the child's tip *is* the rewound sequence, so the
                    // projection, the stream and the kernel agree again.
                    //
                    // A rejected sequence (0, or past the session's last) is
                    // the ordinary mistake here and stays a local command
                    // error rather than the end of the session.
                    let parent_id = self.session_id;
                    let forked = block_on(
                        self.client.fork_session(ForkSession::new(
                            parent_id,
                            to_seq,
                            self.actor.clone(),
                            TraceId::new(),
                        )),
                        self.cancel,
                    );
                    match forked {
                        Ok(child) => {
                            let child_id = child.id();
                            self.switch_to_session(child_id)?;
                            self.append_command_output(format!(
                                "rewound to seq {to_seq}: now on session {child_id}\n\
workspace files are not restored by a rewind; `/resume {parent_id}` returns to \
the full history, where `/diff` lists every file it wrote\n"
                            ));
                        }
                        // A cancelled block_on is the session shutting down,
                        // not a bad sequence: that one still propagates.
                        Err(InteractiveError::Cancelled) => {
                            return Err(InteractiveError::Cancelled);
                        }
                        Err(err) => {
                            self.append_command_error(format!("rewind to {to_seq} failed: {err}"));
                        }
                    }
                }
                // Unreachable: `kernel_action_is_supported` refuses every
                // `Approve`/`Dispatch` action before the match is entered.
                // Kept as the same honest message rather than a panic, so a
                // predicate that ever disagreed with this match degrades to
                // "not available" instead of killing the session.
                KernelApi::Approve | KernelApi::Dispatch => {
                    self.append_command_error(unsupported_command_text(&other));
                }
            },
        }
        self.drain()
    }

    /// The session's current sequence as the ledger has it — the number an
    /// optimistic `expected_seq` must name. The UI projection's own seq lags
    /// this by the live-tail poll interval and must not be used for it.
    fn session_tip(&self) -> Result<u64, InteractiveError> {
        Ok(block_on(self.client.get_session(self.session_id), self.cancel)?.seq())
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
        if self.model_busy() {
            return Ok(());
        }
        // The kernel's own tip, not the projection's seq. The projection
        // lags the ledger by the live-tail poll interval (see
        // `drain_kernel_events`), and a turn that just finished on its own
        // thread appended its terminal events *after* the last drain and
        // *before* clearing `turn_in_flight` — so the projection's seq is
        // stale in exactly the window a user (or the autonomous loop)
        // submits next. That stale seq hit `SessionConflict`, and the
        // error ended the whole session. `expected_seq` exists to catch a
        // concurrent *writer*, and this session's own lag is not one; the
        // tip read here still catches a genuine one between this read and
        // the submit. Found by the first CI run on a shared macOS runner,
        // where the window is wide enough to hit every time.
        let expected_seq = self.session_tip()?;
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
                    self.jobs.clone(),
                    self.shared.clone(),
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
                self.jobs.clone(),
                self.shared.clone(),
            );
        }
        self.drain()
    }

    /// Whether the session's one model slot is taken — by a turn on its
    /// thread or a compaction on its own. One model call per session at a
    /// time: a turn and a compaction would otherwise race to read and
    /// rewrite the same history.
    fn model_busy(&self) -> bool {
        self.turn_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            || self.compaction.is_some()
    }

    /// `/compact`: fold the session's earlier turns into a model-written
    /// summary, recorded as `context.compacted` so every later turn — in
    /// this process, in a `rapid exec --continue`, in a resumed session —
    /// reads the summary in place of the turns it covers.
    ///
    /// Runs on its own thread the way a turn does, so the loop keeps
    /// painting and Ctrl-C reaches it — but not *as* a turn: no
    /// `turn.started`, no lease, nothing in the transcript's user column,
    /// because the user asked for maintenance, not an answer. The turn
    /// slot is held for the duration (`model_busy`), and the thread reports
    /// back through `settle_compaction`.
    fn compact_session(&mut self) -> Result<(), InteractiveError> {
        if self.refuse_if_busy("compacting")? {
            return Ok(());
        }
        // The history is read on the thread too — a turn reads it on its
        // own thread for the same reason: the read walks the ledger, and
        // the loop must keep painting.
        let cancel = agent_runtime::CancellationToken::new();
        let outcome = CompactionOutcomeSlot::default();
        #[cfg(test)]
        let scripted = self
            .scripted_backings
            .as_ref()
            .and_then(|queue| queue.lock().unwrap_or_else(|p| p.into_inner()).pop_front());
        #[cfg(test)]
        if let Some(backing) = scripted {
            spawn_compaction_with_backing(
                self.client.clone(),
                self.session_id,
                self.actor.clone(),
                self.root.to_path_buf(),
                self.trusted,
                cancel.clone(),
                std::sync::Arc::clone(&outcome),
                backing,
                (
                    crate::user_config::DEFAULT_CONTEXT_WINDOW,
                    crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
                ),
            );
            self.compaction = Some(CompactionInFlight { cancel, outcome });
            self.append_command_output("compacting... (Ctrl-C cancels)".to_owned());
            return self.drain();
        }
        spawn_compaction(
            self.client.clone(),
            self.session_id,
            self.actor.clone(),
            self.root.to_path_buf(),
            self.trusted,
            cancel.clone(),
            std::sync::Arc::clone(&outcome),
            self.shared.clone(),
        );
        self.compaction = Some(CompactionInFlight { cancel, outcome });
        self.append_command_output("compacting... (Ctrl-C cancels)".to_owned());
        self.drain()
    }

    /// Move what the threads left in [`SessionNotices`] into the transcript.
    fn surface_notices(&mut self) {
        let pending: Vec<String> = std::mem::take(
            &mut *self
                .shared
                .notices
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for line in pending {
            self.append_command_output(line);
        }
    }

    /// Collect a finished compaction's report and free the model slot. The
    /// summary itself arrives through the ordinary subscription, as the
    /// `context.compacted` event the thread appended; this says what the
    /// thread found, and when it did not get that far.
    fn settle_compaction(&mut self) {
        let finished = match &self.compaction {
            Some(in_flight) => in_flight
                .outcome
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(),
            None => return,
        };
        let Some(result) = finished else {
            return;
        };
        self.compaction = None;
        match result {
            Ok(CompactionReport::Compacted { turns, tokens }) => {
                self.append_command_output(format!(
                    "compacted {turns} turn(s) in {tokens} tokens; later turns carry the summary"
                ));
            }
            Ok(CompactionReport::NothingToCompact { compacted_before }) => {
                self.append_command_output(if compacted_before {
                    "nothing to compact: no turns since the last compaction".to_owned()
                } else {
                    "nothing to compact: the session has no completed turns yet".to_owned()
                });
            }
            Err(reason) => self.append_command_error(format!("compaction failed: {reason}")),
        }
    }

    fn interrupt(&mut self) -> Result<(), InteractiveError> {
        interrupt_session(self.client, self.session_id, self.actor, self.cancel)?;
        *self.interrupt_count = self.interrupt_count.saturating_add(1);
        Ok(())
    }

    fn drain(&mut self) -> Result<(), InteractiveError> {
        self.settle_compaction();
        self.surface_notices();
        drain_kernel_events(
            self.client,
            self.stream,
            self.ui,
            self.session_id,
            self.cancel,
        )?;
        self.refresh_job_logs();
        self.renderer
            .render(self.ui)
            .map_err(|_| InteractiveError::Io)
    }

    /// Re-read an open `/jobs logs` view from the spool before painting.
    ///
    /// Without this the view is whatever the command captured at the instant
    /// it ran, so watching a running build — the reason to open it at all —
    /// would show a frozen page under a header that says `[started]`. The
    /// spool is the same buffer `job_output` serves the model from, so the
    /// two readers stay in step.
    ///
    /// Costs nothing on an idle session: a job that has stopped can never
    /// add to its spool again, so a page read *after* it stopped is final
    /// and never re-read. A page read while it ran is re-read every tick,
    /// and once more on the tick that sees it stop — the lines the job
    /// wrote between the last live read and its exit are on that final
    /// page and nowhere else. Skipping that read left a finished job's
    /// last output off the screen for good (CI's Linux runner, where the
    /// job's exit and its last line landed inside one tick).
    fn refresh_job_logs(&mut self) {
        let Some(view) = self.ui.job_logs() else {
            return;
        };
        if view.is_complete() {
            return;
        }
        let job = view.job();
        let still_running = self
            .ui
            .jobs()
            .get(&job)
            .is_some_and(|projected| !projected.state().is_terminal());
        let refreshed = self.job_log_page(job);
        let refreshed = if still_running {
            refreshed
        } else {
            refreshed.completed()
        };
        *self.ui = reduce(
            self.ui.clone(),
            &UiEvent::Local(LocalUiEvent::SyncJobLogs(Some(refreshed))),
        );
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

/// A headless run's own session in the project ledger.
///
/// `rapid exec` used to mint a `SessionId` for its execution request and
/// never open the ledger: a run that wrote files and started jobs was not in
/// `rapid sessions list`, could not be `rapid resume`d, and had no `/diff`.
/// The interactive path recorded all of it. This is the same session, turn
/// and sinks the TUI uses, opened for one turn.
///
/// Fail-open: a project whose ledger cannot be opened still gets its run —
/// recording is a record, not a precondition — and the user is told the run
/// was not recorded rather than left to discover an absent session.
struct ExecRecording {
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
    /// The session tip after creation, which `SubmitTurn` must name.
    seq: u64,
}

impl ExecRecording {
    fn open(root: &Path) -> Result<Self, String> {
        let ledger_path = project_ledger_path(&root.join(PROJECT_MARKER));
        if let Some(parent) = ledger_path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
        }
        let client = InProcessKernelClient::open(&ledger_path)
            .map_err(|err| format!("{}: {err}", ledger_path.display()))?;
        let actor = human_actor().map_err(|err| err.to_string())?;
        let cancel = CancellationToken::new();
        let snapshot = block_on(
            client.create_session(CreateSession::new(
                ProjectId::new(),
                actor.clone(),
                TraceId::new(),
            )),
            &cancel,
        )
        .map_err(|err| err.to_string())?;
        Ok(Self {
            client,
            session_id: snapshot.id(),
            actor,
            seq: snapshot.seq(),
        })
    }

    /// Continue a session already in this project's ledger: `--resume <id>`
    /// or `--continue`. The turn is submitted at the session's current tip,
    /// exactly as an interactive turn on a resumed session is.
    /// `UnknownSession` names an id the ledger has never seen, so the caller
    /// can offer the ids it does have; `Usage` is `--continue` with nothing
    /// recorded yet. A ledger that cannot be opened at all says why on
    /// stderr and is `Io`.
    fn open_existing(root: &Path, resume: &ExecResume) -> Result<Self, InteractiveError> {
        let ledger_path = project_ledger_path(&root.join(PROJECT_MARKER));
        let session_id = match resume {
            ExecResume::Fresh => return Err(InteractiveError::Internal),
            ExecResume::Session(id) => *id,
            ExecResume::MostRecent => {
                most_recent_session(&ledger_path)?.ok_or(InteractiveError::Usage)?
            }
        };
        let client = InProcessKernelClient::open(&ledger_path).map_err(|err| {
            eprintln!(
                "rapid exec: cannot open this project's sessions: {}: {err}",
                ledger_path.display()
            );
            InteractiveError::Io
        })?;
        let actor = human_actor()?;
        let cancel = CancellationToken::new();
        let snapshot = match block_on(client.get_session(session_id), &cancel) {
            Ok(snapshot) => snapshot,
            Err(InteractiveError::Kernel(api))
                if api.code() == protocol::ErrorCode::SessionNotFound =>
            {
                return Err(InteractiveError::UnknownSession(session_id));
            }
            Err(err) => return Err(err),
        };
        Ok(Self {
            client,
            session_id,
            actor,
            seq: snapshot.seq(),
        })
    }

    /// Record the turn's start — the kernel's own `SubmitTurn`, which is
    /// what carries the prompt into the ledger — and hand back the turn id
    /// `finish_turn` needs.
    fn start_turn(&self, text: &str) -> Result<protocol::TurnId, String> {
        let cancel = CancellationToken::new();
        let handle = block_on(
            self.client.submit_turn(SubmitTurn::new(
                self.session_id,
                self.seq,
                self.actor.clone(),
                TraceId::new(),
                text,
            )),
            &cancel,
        )
        .map_err(|err| err.to_string())?;
        Ok(handle.turn_id())
    }

    fn sink(&self) -> InteractiveTurnSink<'_> {
        InteractiveTurnSink {
            client: &self.client,
            session_id: self.session_id,
            actor: &self.actor,
        }
    }

    fn finish_turn<E: std::fmt::Display>(
        &self,
        turn_id: protocol::TurnId,
        run_result: &Result<crate::host::ExecOutcome, E>,
        history_through: u64,
    ) {
        if let Ok(outcome) = run_result {
            record_turn_context(
                &self.client,
                self.session_id,
                &self.actor,
                outcome,
                history_through,
            );
        }
        let _ = self.client.finish_turn(kernel::FinishTurn::new(
            self.session_id,
            turn_id,
            self.actor.clone(),
            TraceId::new(),
            kernel_turn_outcome(run_result),
        ));
    }
}

/// The headless turn's event sink: every event into the run's own `Vec`, and
/// — when the run is recorded — the same event into the ledger through the
/// same `InteractiveTurnSink` an interactive turn uses. One sink type for
/// both cases, so `run_live_exec`'s two call sites in `exec_turn` need no
/// branching.
struct RecordedEvents<'a> {
    events: &'a mut Vec<agent_runtime::TurnEvent>,
    ledger: Option<InteractiveTurnSink<'a>>,
}

impl agent_runtime::TurnEventSink for RecordedEvents<'_> {
    fn emit(&mut self, event: agent_runtime::TurnEvent) -> Result<(), agent_runtime::TurnError> {
        if let Some(ledger) = self.ledger.as_mut() {
            ledger.emit(event.clone())?;
        }
        self.events.emit(event)
    }
}

impl agent_runtime::TurnEventSink for InteractiveTurnSink<'_> {
    fn emit(&mut self, event: agent_runtime::TurnEvent) -> Result<(), agent_runtime::TurnError> {
        use agent_runtime::TurnEvent;
        use event_ledger::event::EventKind;
        // Set by the one arm that carries one; folded into the payload below
        // rather than widening the tuple every other arm would have to pad.
        let mut denial_reason: Option<String> = None;
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
            TurnEvent::ModelFailed {
                turn_id,
                request_id,
            } => (
                EventKind::ModelFailed,
                turn_id,
                None,
                None,
                Some(request_id),
                None,
                None,
            ),
            TurnEvent::ToolRequested {
                turn_id,
                call_id,
                tool,
            } => (
                EventKind::ToolRequested,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            TurnEvent::ToolStarted {
                turn_id,
                call_id,
                tool,
            } => (
                EventKind::ToolStarted,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            TurnEvent::ToolCompleted {
                turn_id,
                call_id,
                tool,
            } => (
                EventKind::ToolCompleted,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            TurnEvent::ToolFailed {
                turn_id,
                call_id,
                tool,
            } => (
                EventKind::ToolFailed,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            // The only tool event that carries a reason: it is what tells a
            // user *why* a call was refused and what to do about it, and it
            // reached only the model before this.
            TurnEvent::ToolDenied {
                turn_id,
                call_id,
                tool,
                reason,
            } => {
                denial_reason = reason;
                (
                    EventKind::ToolDenied,
                    turn_id,
                    Some(call_id),
                    Some(tool),
                    None,
                    None,
                    None,
                )
            }
            TurnEvent::ToolApprovalRequired {
                turn_id,
                call_id,
                tool,
            } => (
                EventKind::ToolApprovalRequired,
                turn_id,
                Some(call_id),
                Some(tool),
                None,
                None,
                None,
            ),
            TurnEvent::ToolContextRequired {
                turn_id,
                call_id,
                tool,
            } => (
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
            // Read back by `tui::state`'s fold through the same bounded,
            // redaction-aware accessor as `tool`.
            "detail": denial_reason,
        });
        self.client
            .append_turn_progress(self.session_id, self.actor, TraceId::new(), kind, payload)
            .map_err(|_| agent_runtime::TurnError::EventSink)
    }
}

/// Reports a turn's background jobs into the session ledger, so the `/jobs`
/// panel shows what `shell_exec background=true` actually started.
///
/// Owns its handles rather than borrowing: a job outlives the tool call that
/// started it (that is the point of a background job), and its supervisor
/// thread reports completion long after `execute_interactive_turn` has
/// returned. `InProcessKernelClient` is `Clone` and appends through the
/// ledger's own serialized transaction, and `append_turn_progress` writes at
/// the session tip with no optimistic check — which is exactly right for a
/// writer racing the turn's own events, and is why the same call is what the
/// turn sink uses.
///
/// A failed append is dropped: the job itself is real work that must not be
/// disturbed by the ledger, and the model-facing `job_status`/`job_output`
/// tools remain the authority either way.
struct LedgerJobEvents {
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
}

/// A `task_spawn` child's lifecycle as `agent.*` ledger events — what the
/// `/agents` panel and the kernel's own `active_agents` project. One
/// `agent.spawned` and exactly one terminal event per child, in that
/// order: the projection refuses a terminal event for an agent it does
/// not have, and a session that replays into a refused event is unreadable.
struct LedgerAgentEvents {
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
}

impl crate::exec_tools::AgentEvents for LedgerAgentEvents {
    fn spawned(&self, agent: protocol::AgentId, agent_type: &str, task: &str) -> bool {
        // Whether it landed matters here as it does not for a job's start:
        // the projection refuses `agent.result`/`agent.cancelled` for an
        // agent it never saw spawned, and a refused event makes the
        // session unreadable on every replay after it.
        self.client
            .append_turn_progress(
                self.session_id,
                &self.actor,
                TraceId::new(),
                event_ledger::event::EventKind::AgentSpawned,
                serde_json::json!({
                    "agent_id": agent.to_string(),
                    "role": agent_type,
                    "state": "running",
                    "current_operation": task,
                }),
            )
            .is_ok()
    }

    fn finished(
        &self,
        agent: protocol::AgentId,
        end: crate::exec_tools::SubagentEnd,
        detail: Option<&str>,
    ) {
        use crate::exec_tools::SubagentEnd;
        use event_ledger::event::EventKind;
        let (kind, payload) = match end {
            SubagentEnd::Succeeded => (
                EventKind::AgentResult,
                serde_json::json!({"agent_id": agent.to_string(), "state": "succeeded"}),
            ),
            // `agent.state_changed` with a terminal state is how the
            // projection records a failure (there is no `agent.failed`).
            SubagentEnd::Failed => (
                EventKind::AgentStateChanged,
                serde_json::json!({
                    "agent_id": agent.to_string(),
                    "state": "failed",
                    "blocker": detail.unwrap_or("failed"),
                }),
            ),
            SubagentEnd::Cancelled => (
                EventKind::AgentCancelled,
                serde_json::json!({"agent_id": agent.to_string(), "state": "cancelled"}),
            ),
        };
        let _ = self.client.append_turn_progress(
            self.session_id,
            &self.actor,
            TraceId::new(),
            kind,
            payload,
        );
    }
}

impl crate::exec_tools::JobEvents for LedgerJobEvents {
    fn started(&self, job: protocol::JobId, handle: &str, command: &str) {
        let _ = self.client.append_turn_progress(
            self.session_id,
            &self.actor,
            TraceId::new(),
            event_ledger::event::EventKind::JobStarted,
            serde_json::json!({
                "job_id": job.to_string(),
                "state": "started",
                // The short id the model was given, so a reader can match a
                // panel row to what the transcript said, and the argv, so
                // the row means something without either.
                "handle": handle,
                "command": command,
            }),
        );
    }

    fn finished(&self, job: protocol::JobId, state: &str, exit_status: Option<i32>) {
        let _ = self.client.append_turn_progress(
            self.session_id,
            &self.actor,
            TraceId::new(),
            event_ledger::event::EventKind::JobCompleted,
            serde_json::json!({
                "job_id": job.to_string(),
                "state": state,
                "exit_status": exit_status,
            }),
        );
    }
}

/// Reports a turn's workspace writes into the session ledger, so `/diff`
/// shows what the agent changed.
///
/// Owns its handles for the same reason [`LedgerJobEvents`] does — a
/// subagent's writes come from a different call stack — and appends through
/// the same `append_turn_progress`, at the session tip, racing the turn's own
/// events safely. A failed append is dropped: the write already happened and
/// is the real work; the ledger not hearing about it must not turn a
/// successful edit into a tool failure.
struct LedgerWorkspaceChanges {
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
}

impl crate::exec_tools::WorkspaceChanges for LedgerWorkspaceChanges {
    fn wrote(&self, path: &str, before: Option<u64>, after: u64, hunks: Option<&str>) {
        let _ = self.client.append_turn_progress(
            self.session_id,
            &self.actor,
            TraceId::new(),
            event_ledger::event::EventKind::WorkspaceMutationDetected,
            serde_json::json!({
                "path": path,
                // Absent means the file did not exist, which the panel shows
                // as "new" rather than as a change from zero lines.
                "lines_before": before,
                "lines_after": after,
                // Absent when no diff was computed. Bounded by the producer
                // at `line_diff::MAX_UNIFIED_BYTES`, well under the reducer's
                // display bound — a payload field over that bound is a
                // protocol error that freezes the session.
                "hunks": hunks,
            }),
        );
    }
}

/// Run `f`, converting a panic into a `Failed` outcome instead of letting it
/// unwind past whatever the caller does afterward — `spawn_interactive_
/// turn`'s cleanup (releasing the turn's lease, clearing `turn_in_flight`)
/// must run regardless of how execution ends, panic included, or a panic
/// deep in `run_live_exec` would strand the lease *and* leave the session
/// silently unresponsive to every later message for the rest of the
/// process (found in an adversarial self-review of this feature).
fn catching_panics(
    f: impl FnOnce() -> kernel::TurnOutcome + std::panic::UnwindSafe,
) -> kernel::TurnOutcome {
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
    // The *session's* job table, so a background job outlives the turn that
    // started it. See `JobRegistry::share_table`.
    jobs: crate::exec_tools::JobRegistry,
    shared: SessionShared,
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
            run_interactive_turn(
                &client,
                session_id,
                &actor,
                &root,
                trusted,
                &text,
                &kernel_cancel,
                &jobs,
                &shared,
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
    // The session's job table — see `spawn_interactive_turn`'s own parameter.
    jobs: crate::exec_tools::JobRegistry,
    shared: SessionShared,
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
                &jobs,
                &shared,
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

#[allow(clippy::too_many_arguments)]
fn run_interactive_turn(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    kernel_cancel: &kernel::CancelToken,
    jobs: &crate::exec_tools::JobRegistry,
    shared: &SessionShared,
) -> kernel::TurnOutcome {
    let bridge = CancelBridge::start(kernel_cancel);
    let outcome = run_interactive_turn_inner(
        client,
        session_id,
        actor,
        root,
        trusted,
        text,
        &bridge.token,
        jobs,
        shared,
    );
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
    jobs: &crate::exec_tools::JobRegistry,
    shared: &SessionShared,
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
        jobs,
        shared,
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
    context_limit: u32,
    output_reserve: u32,
    history: crate::host::ConversationHistory,
    reminder_block: Option<String>,
) -> Result<PreservedLiveContext, kernel::TurnOutcome> {
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
    let preserved = preserve_memory_and_todos(preserved, root)
        .with_conversation(history.turns)
        .with_compaction_summary(history.summary)
        .with_reminders_block(reminder_block);
    // Proactive context retrieval, as `exec_turn` does it: only for a
    // trusted project (it walks the tree and writes an incremental index
    // under .rapidlm/index/), and failing open inside retrieve() itself.
    Ok(if trusted {
        let retrieved = crate::context_retrieval::retrieve(root, text, RETRIEVAL_BUDGET_TOKENS);
        preserved.with_retrieved_context(retrieved)
    } else {
        preserved
    })
}

/// The tools half of one interactive turn: the permission lattice (mode,
/// project-settings rules, persisted grants — `forced_mode` overriding every
/// other mode source, see `run_interactive_turn_inner_with_backing`), and
/// workspace tools only for a trusted project. Everything a trusted
/// project's settings add on top is `configure_trusted_tools`, once the
/// model is known.
fn build_interactive_turn_tools(
    root: &Path,
    trusted: bool,
    forced_mode: Option<crate::permissions::PermissionMode>,
) -> Result<(ExecTools, crate::permissions::PermissionLattice), kernel::TurnOutcome> {
    let permission_lattice = match exec_permission_lattice(Some(root), forced_mode) {
        Ok(lattice) => lattice,
        Err(err) => {
            return Err(kernel::TurnOutcome::Failed {
                reason: format!("permission configuration error: {err}"),
            });
        }
    };
    let tools = if trusted {
        ExecTools::workspace_with_permissions(root, permission_lattice.clone())
            .unwrap_or_else(|_| ExecTools::noop())
    } else {
        ExecTools::noop()
    };
    Ok((tools, permission_lattice))
}

/// The reminder roster's block and floor for an interactive turn, read the
/// way `exec_turn` reads them: a broken roster warns and the turn continues
/// without reminders.
fn interactive_reminders(
    root: &Path,
    warn: &mut dyn FnMut(&str),
) -> (agent_runtime::reminders::ReminderFloor, Option<String>) {
    match load_active_reminders(Some(root)) {
        Ok(Some((block, floor))) => (floor, Some(block)),
        Ok(None) => (agent_runtime::reminders::ReminderFloor::Baseline, None),
        Err(err) => {
            warn(&format!("warning: reminders not loaded: {err}"));
            (agent_runtime::reminders::ReminderFloor::Baseline, None)
        }
    }
}

/// Actually resolve a model, build workspace tools, and run one turn through
/// the same `run_live_exec` entry the headless `rapid exec` path uses — and
/// on the same setup: the `[models] fallback` chain and managed-policy
/// ceilings, the project's hooks, MCP servers, reminders and proactive
/// retrieval, the memory and todo indices, the subagent runner. Each piece
/// is the function `exec_turn` calls for the same thing.
#[allow(clippy::too_many_arguments)]
fn run_interactive_turn_inner(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    text: &str,
    cancel: &agent_runtime::CancellationToken,
    jobs: &crate::exec_tools::JobRegistry,
    shared: &SessionShared,
) -> kernel::TurnOutcome {
    let mut warn = |line: &str| notify(&shared.notices, line);
    let (mut tools, permission_lattice) = match build_interactive_turn_tools(root, trusted, None) {
        Ok(built) => built,
        Err(outcome) => return outcome,
    };
    let policy_version = apply_managed_ceilings(&mut tools);
    // The session's MCP connections, before the integrations connect any:
    // a server the session already has is reused, not spawned again — and
    // its subagent registry, so `/agents cancel` reaches a child this turn
    // starts.
    tools.share_mcp(&shared.mcp);
    tools.share_subagents(&shared.agents);
    // Session-start/end hooks are per run; the interactive session fires
    // its own at start and exit, not per turn.
    if trusted {
        let _ = configure_trusted_integrations(&mut tools, root, &mut warn);
    }
    // Reminders ahead of the model: the floor picks its reasoning effort.
    let (reminder_floor, reminder_block) = interactive_reminders(root, &mut warn);
    // The model, resolved the way a headless run resolves it — env
    // overrides, user config, the `[models] fallback` chain, the managed
    // policy — and *before* context construction, so the context budget is
    // derived from the model that will actually run this turn.
    let session_model = match SessionModel::resolve(reminder_floor, policy_version, &mut warn) {
        Ok(model) => model,
        Err(reason) => return kernel::TurnOutcome::Failed { reason },
    };
    let backing = match session_model.backing(None, &mut warn) {
        Ok(backing) => backing,
        Err(reason) => return kernel::TurnOutcome::Failed { reason },
    };
    let (context_limit, output_reserve) = context_budget_for(&backing);

    // The session's earlier turns, so "now fix the tests" on turn two means
    // what it says: without this every turn ran on its prompt alone, and
    // the model had no idea what the previous turn had been. Read from the
    // ledger, which is what the resumed transcript is rebuilt from too.
    let history = conversation_history(client, session_id, cancel);
    let history_through = history.through_seq;
    let preserved = match build_interactive_turn_context(
        root,
        trusted,
        text,
        context_limit,
        output_reserve,
        history,
        reminder_block,
    ) {
        Ok(preserved) => preserved,
        Err(outcome) => return outcome,
    };
    configure_trusted_model_tools(
        &mut tools,
        root,
        session_model.primary(),
        &permission_lattice,
        Some(LedgerSinks {
            client,
            session_id,
            actor,
        }),
    );
    // Background jobs go in the session's table, not this turn's: see
    // `SessionLoop::jobs`.
    tools.share_job_table(jobs);

    execute_interactive_turn(
        client,
        session_id,
        actor,
        root,
        text,
        preserved,
        &mut tools,
        backing,
        cancel,
        history_through,
    )
}

/// The session's model as one interactive turn (or a `/compact`) resolves
/// it: the same `resolve_model_plan`/`build_backing_model` pair a headless
/// run and `rapid doctor` use — env overrides, user config, the `[models]
/// fallback` chain, the managed policy — so the TUI runs on exactly the
/// model configuration the headless path would. Owns the credential stores
/// the backing borrows from.
struct SessionModel {
    models: Vec<crate::user_config::ActiveModel>,
    stores: Vec<auth::InMemoryCredentialStore>,
    primary: Option<crate::user_config::ActiveModel>,
    /// `[phases] compact`, when it routes elsewhere — with its own store.
    compact: Option<(
        crate::user_config::ActiveModel,
        auth::InMemoryCredentialStore,
    )>,
    unconfigured: bool,
    policy_version: Option<String>,
}

impl SessionModel {
    /// The error is the turn-failure reason text.
    fn resolve(
        reminder_floor: agent_runtime::reminders::ReminderFloor,
        policy_version: Option<String>,
        warn: &mut dyn FnMut(&str),
    ) -> Result<Self, String> {
        let process_env: Vec<(String, String)> = std::env::vars().collect();
        let plan = resolve_model_plan(&process_env, reminder_floor, warn)
            .map_err(|err| err.to_string())?;
        let stores = plan
            .models
            .iter()
            .map(|_| auth::InMemoryCredentialStore::new())
            .collect();
        Ok(Self {
            models: plan.models,
            stores,
            primary: plan.primary_config,
            compact: plan
                .compact
                .map(|active| (active, auth::InMemoryCredentialStore::new())),
            unconfigured: plan.unconfigured,
            policy_version,
        })
    }

    /// The model a `/compact` runs on: the `[phases] compact` override when
    /// one is configured and allowed, else the conversation model (with its
    /// fallback chain). The in-turn overflow recovery always uses the turn's
    /// own model — it summarises with the backing it already holds.
    fn compaction_backing(&self, warn: &mut dyn FnMut(&str)) -> Result<SelectedModel<'_>, String> {
        if let Some((active, store)) = &self.compact {
            return ConfiguredModel::build(active, store)
                .map(|model| SelectedModel::Configured(Box::new(model)))
                .map_err(|err| format!("model configuration error (phases.compact): {err}"));
        }
        self.backing(None, warn)
    }

    /// The configured primary model — what subagents run on and whose
    /// credential is scrubbed from captured output.
    fn primary(&self) -> Option<&crate::user_config::ActiveModel> {
        self.primary.as_ref()
    }

    /// Build the backing. The routing-decision log a fallback chain keeps
    /// is dropped here: the interactive session has nowhere to surface it
    /// yet (headless writes it to `--jsonl`).
    fn backing(
        &self,
        diag: Option<StepDiag>,
        warn: &mut dyn FnMut(&str),
    ) -> Result<SelectedModel<'_>, String> {
        build_backing_model(
            &self.models,
            &self.stores,
            self.unconfigured,
            diag,
            self.policy_version.clone(),
            warn,
        )
        .map(|(backing, _decisions)| backing)
        .map_err(|err| err.to_string())
    }
}

/// Spawn the thread a `/compact` runs on: resolve the session's model, write
/// the summary, record it. Reports through `outcome` on every path — a
/// panic included, so the loop's model slot is always freed (the same
/// guarantee `spawn_interactive_turn` gives the turn lease).
#[allow(clippy::too_many_arguments)]
fn spawn_compaction(
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
    root: PathBuf,
    trusted: bool,
    cancel: agent_runtime::CancellationToken,
    outcome: CompactionOutcomeSlot,
    shared: SessionShared,
) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut warn = |line: &str| notify(&shared.notices, line);
            let session_model = SessionModel::resolve(
                agent_runtime::reminders::ReminderFloor::Baseline,
                None,
                &mut warn,
            )?;
            let backing = session_model.compaction_backing(&mut warn)?;
            let budget = context_budget_for(&backing);
            run_compaction(
                &client, session_id, &actor, &root, trusted, backing, budget, &cancel,
            )
        }))
        .unwrap_or_else(|_| Err("compaction panicked".to_owned()));
        *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
    });
}

/// Test-only-but-real sibling of `spawn_compaction`: the same thread and
/// reporting shape around an already-resolved `backing` and its budget.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn spawn_compaction_with_backing<B: crate::host::LiveModelCall + Send + 'static>(
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
    root: PathBuf,
    trusted: bool,
    cancel: agent_runtime::CancellationToken,
    outcome: CompactionOutcomeSlot,
    backing: B,
    budget: (u32, u32),
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_compaction(
                &client, session_id, &actor, &root, trusted, backing, budget, &cancel,
            )
        }))
        .unwrap_or_else(|_| Err("compaction panicked".to_owned()));
        *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
    })
}

/// One compaction, end to end: the history read from the ledger, the
/// summary through the shared `run_live_compaction` entry, its tokens
/// accrued to the active goal (a model call the goal's budget must see,
/// though not a turn), and the `context.compacted` event appended at the
/// session's tip — the record every later turn reads the summary from. The
/// error is the text the loop shows.
#[allow(clippy::too_many_arguments)]
fn run_compaction<B: crate::host::LiveModelCall>(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    root: &Path,
    trusted: bool,
    backing: B,
    budget: (u32, u32),
    cancel: &agent_runtime::CancellationToken,
) -> Result<CompactionReport, String> {
    let history = conversation_history(client, session_id, cancel);
    // The read stops early when cancelled, and an early-stopped read looks
    // like an empty one: say cancelled, not "nothing to compact".
    if cancel.is_cancelled() {
        return Err(
            crate::host::CompactionError::Model(agent_runtime::ModelStepError::Cancelled)
                .to_string(),
        );
    }
    if history.turns.is_empty() {
        return Ok(CompactionReport::NothingToCompact {
            compacted_before: history.summary.is_some(),
        });
    }
    // `pre_compact`/`post_compact` hooks: project settings run shell
    // commands, so a trusted project only — the same gate every other hook
    // is behind. Notification-style: they observe, never gate.
    let hooks = if trusted {
        load_project_integrations(root).hooks
    } else {
        crate::hooks::HooksConfig::default()
    };
    if !hooks.pre_compact.is_empty() {
        let _ = crate::hooks::run_notify_hooks(
            &hooks.pre_compact,
            "pre_compact",
            serde_json::json!({
                "session_id": session_id.to_string(),
                "turns": history.turns.len(),
                "trigger": "manual",
            }),
            crate::hooks::HOOK_TIMEOUT,
        );
    }
    let (context_limit, output_reserve) = budget;
    let summary = crate::host::run_live_compaction(
        backing,
        &history,
        context_limit,
        output_reserve,
        cancel,
        None,
    )
    .map_err(|err| err.to_string())?;
    // Spent whether or not the record below lands.
    let goal_path = root.join(PROJECT_MARKER).join(GOAL_FILE);
    if let Some(goal_id) = active_goal_id(&goal_path) {
        accrue_model_usage(
            &goal_path,
            goal_id,
            summary.tokens,
            summary.cost_usd_micros.unwrap_or(0),
        );
    }
    record_compaction(
        client,
        session_id,
        actor,
        &summary.text,
        history.through_seq,
        summary.turns,
        serde_json::json!({
            "tokens": summary.tokens,
            "cost_usd_micros": summary.cost_usd_micros,
            "source": "compact",
        }),
    )?;
    if !hooks.post_compact.is_empty() {
        let _ = crate::hooks::run_notify_hooks(
            &hooks.post_compact,
            "post_compact",
            serde_json::json!({
                "session_id": session_id.to_string(),
                "turns": summary.turns,
                "summary_bytes": summary.text.len(),
                "tokens": summary.tokens,
                "trigger": "manual",
            }),
            crate::hooks::HOOK_TIMEOUT,
        );
    }
    Ok(CompactionReport::Compacted {
        turns: summary.turns,
        tokens: summary.tokens,
    })
}

/// Append `context.compacted`: `summary` stands for every turn of this
/// session recorded through `through_seq` (and every turn of the sessions
/// it was forked from). `extra` carries the writer's own fields — what the
/// call cost, which path wrote it.
fn record_compaction(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    summary: &str,
    through_seq: u64,
    turns: usize,
    extra: serde_json::Value,
) -> Result<(), String> {
    let mut payload = serde_json::json!({
        "summary": summary,
        "through_seq": through_seq,
        "turns": turns,
    });
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    client
        .append_turn_progress(
            session_id,
            actor,
            TraceId::new(),
            event_ledger::event::EventKind::ContextCompacted,
            payload,
        )
        .map_err(|err| format!("the summary was written but could not be recorded: {err}"))
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
    jobs: &crate::exec_tools::JobRegistry,
    shared: &SessionShared,
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
    let mut warn = |line: &str| notify(&shared.notices, line);
    let (mut tools, permission_lattice) =
        match build_interactive_turn_tools(root, trusted, forced_mode) {
            Ok(built) => built,
            Err(outcome) => return outcome,
        };
    let _policy_version = apply_managed_ceilings(&mut tools);
    tools.share_mcp(&shared.mcp);
    tools.share_subagents(&shared.agents);
    if trusted {
        let _ = configure_trusted_integrations(&mut tools, root, &mut warn);
    }
    let (_reminder_floor, reminder_block) = interactive_reminders(root, &mut warn);
    let history = conversation_history(client, session_id, cancel);
    let history_through = history.through_seq;
    let preserved = match build_interactive_turn_context(
        root,
        trusted,
        text,
        context_limit,
        output_reserve,
        history,
        reminder_block,
    ) {
        Ok(preserved) => preserved,
        Err(outcome) => return outcome,
    };
    // No configured model behind a scripted backing: no subagent runner
    // and no credential canary, everything else as production.
    configure_trusted_model_tools(
        &mut tools,
        root,
        None,
        &permission_lattice,
        Some(LedgerSinks {
            client,
            session_id,
            actor,
        }),
    );
    // Same session-scoped job table the production path uses.
    tools.share_job_table(jobs);
    if let Some(runner) = &shared.scripted_subagents {
        tools.set_subagent_runner(std::sync::Arc::clone(runner));
    }
    execute_interactive_turn(
        client,
        session_id,
        actor,
        root,
        text,
        preserved,
        &mut tools,
        backing,
        cancel,
        history_through,
    )
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
    // The seq the turn's history was read through — see
    // `record_turn_context`.
    history_through: u64,
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
    if let Ok(outcome) = &run_result {
        record_turn_context(client, session_id, actor, outcome, history_through);
    }
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

    kernel_turn_outcome(&run_result)
}

/// Attach the ledger-backed observers a recorded session needs, so that
/// background jobs reach `/jobs` and workspace writes reach `/diff` through
/// the ordinary subscription rather than a second channel.
///
/// One function for the interactive and the headless path: a headless run
/// used to attach nothing, so `rapid exec` left no record of what it wrote
/// or ran.
fn attach_ledger_sinks(
    tools: &mut ExecTools,
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
) {
    tools.set_job_events(std::sync::Arc::new(LedgerJobEvents {
        client: client.clone(),
        session_id,
        actor: actor.clone(),
    }));
    tools.set_workspace_changes(std::sync::Arc::new(LedgerWorkspaceChanges {
        client: client.clone(),
        session_id,
        actor: actor.clone(),
    }));
    tools.set_agent_events(std::sync::Arc::new(LedgerAgentEvents {
        client: client.clone(),
        session_id,
        actor: actor.clone(),
    }));
}

/// Compiled-context usage, appended on the same terms as a background job's
/// lifecycle: through `append_turn_progress`, at the session tip, so it
/// reaches the status line by the ordinary subscription rather than a second
/// channel. Emitted for every completed turn — a failed turn still filled a
/// context, and the figure is what the model was last given. The per-class
/// rows are for the `/context` panel: the totals cannot say which class is
/// consuming the window.
///
/// A summary the turn's overflow recovery wrote is recorded here too, as
/// `context.compacted` covering the history the turn was given
/// (`history_through`): without it the next turn would read the same turns,
/// overflow on them the same way and pay for the same recovery, silently,
/// on every turn until someone ran `/compact`.
fn record_turn_context(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    outcome: &crate::host::ExecOutcome,
    history_through: u64,
) {
    if let Some(recovered) = &outcome.recovered {
        // Best-effort, like the usage event below: the turn's own outcome
        // does not depend on this landing.
        let _ = record_compaction(
            client,
            session_id,
            actor,
            &recovered.text,
            history_through,
            recovered.turns,
            serde_json::json!({"source": "overflow-recovery"}),
        );
    }
    let Some((used, limit)) = outcome.context_tokens else {
        return;
    };
    let _ = client.append_turn_progress(
        session_id,
        actor,
        TraceId::new(),
        event_ledger::event::EventKind::ContextCompiled,
        serde_json::json!({
            "included_tokens": used,
            "context_limit": limit,
            "partitions": outcome
                .context_partitions
                .iter()
                .map(|(class, used, cap)| {
                    serde_json::json!({"class": class, "used": used, "cap": cap})
                })
                .collect::<Vec<_>>(),
        }),
    );
}

/// Map `run_live_exec`'s terminal status onto the kernel's own
/// `TurnOutcome` — the one mapping the ledger's `turn.*` terminal event is
/// derived from, for an interactive turn and a headless one alike.
fn kernel_turn_outcome<E: std::fmt::Display>(
    run_result: &Result<crate::host::ExecOutcome, E>,
) -> kernel::TurnOutcome {
    match run_result {
        Ok(outcome) => match context_required_question(outcome) {
            Some(question) => kernel::TurnOutcome::Completed {
                text: Some(question.to_owned()),
            },
            None => match outcome.result.status() {
                AgentTerminalStatus::Succeeded => kernel::TurnOutcome::Completed {
                    text: Some(outcome.result.summary().to_owned()),
                },
                AgentTerminalStatus::Cancelled => kernel::TurnOutcome::Interrupted,
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

fn context_required_question(outcome: &crate::host::ExecOutcome) -> Option<&str> {
    if outcome.stop_reason != Some(agent_runtime::TurnStopReason::ContextRequired) {
        return None;
    }
    outcome
        .failure_detail
        .as_ref()
        .map(TurnFailureDetail::error)
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

/// Ceiling on how many replayed events a resumed session folds before its
/// first paint.
///
/// A long-lived session's ledger is unbounded, and the transcript projection
/// itself is already bounded (`AppState` caps its own entries), so replaying
/// everything would spend startup time producing rows that are immediately
/// dropped. Whatever is not folded here is still delivered by the ordinary
/// `drain` on later ticks — this only bounds the *pre-paint* work, and drops
/// nothing: the stream keeps its place, so a history longer than this budget
/// finishes arriving over the following frames instead of all at once.
const MAX_REPLAYED_EVENTS: usize = 4096;

/// How many forks back a transcript is inherited — the same depth
/// `conversation_history` follows for the model, so what the user sees and
/// what the model is given agree on where the past begins.
const MAX_INHERIT_DEPTH: usize = MAX_FORK_DEPTH;

/// Put what was said before `session_id` forked in front of `ui`'s
/// transcript: the parent's transcript through the fork point, and that
/// parent's inheritance before it. A session that was not forked, or whose
/// parent cannot be read, inherits nothing — best-effort, like the model's
/// own history read; nothing here can fail the session.
fn inherit_transcript(
    client: &InProcessKernelClient,
    ui: &mut AppState,
    session_id: protocol::SessionId,
    cancel: &CancellationToken,
) {
    let inherited = inherited_transcript(client, session_id, cancel, MAX_INHERIT_DEPTH);
    if !inherited.is_empty() {
        *ui = reduce(
            ui.clone(),
            &UiEvent::Local(LocalUiEvent::InheritTranscript(inherited)),
        );
    }
}

/// Ceiling on parent events one inheritance folds — a safety bound on a
/// finite ledger, not a working budget: the fold must reach `source_seq`,
/// because the transcript projection keeps its *newest* entries, and
/// stopping short would show a rewound session its oldest turns while the
/// ones just before the rewind point went missing.
const MAX_INHERITED_EVENTS: usize = 1_000_000;

/// The transcript `session_id` inherits: empty unless its first event is
/// `session.forked`, in which case the parent's own inheritance (recursion
/// bounded by `depth`) followed by the parent's events through `source_seq`
/// folded through the same reducer the live session uses — the display
/// projection of the past, carried across exactly as it would have been
/// painted. A parent that cannot be folded to the end says so in the
/// transcript rather than ending it silently.
fn inherited_transcript(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    cancel: &CancellationToken,
    depth: usize,
) -> Vec<TranscriptEntry> {
    inherited_transcript_bounded(client, session_id, cancel, depth, MAX_INHERITED_EVENTS)
}

/// [`inherited_transcript`] with the per-parent event ceiling as a
/// parameter, so a test can show what the ceiling does without a
/// million-event parent.
fn inherited_transcript_bounded(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    cancel: &CancellationToken,
    depth: usize,
    max_events: usize,
) -> Vec<TranscriptEntry> {
    use event_ledger::event::EventKind;
    if depth == 0 || cancel.is_cancelled() {
        return Vec::new();
    }
    let Ok(mut stream) = block_on(
        client.subscribe(SubscribeEvents::new(session_id, 0)),
        cancel,
    ) else {
        return Vec::new();
    };
    let Ok(first) = stream.recv() else {
        return Vec::new();
    };
    if first.kind() != EventKind::SessionForked {
        return Vec::new();
    }
    let parent = first
        .payload()
        .get("parent_session_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse::<protocol::SessionId>().ok());
    let source_seq = first
        .payload()
        .get("source_seq")
        .and_then(serde_json::Value::as_u64);
    let (Some(parent), Some(source_seq)) = (parent, source_seq) else {
        return Vec::new();
    };
    let mut entries = inherited_transcript_bounded(client, parent, cancel, depth - 1, max_events);
    // Never past the parent's own tip: a fork record naming a seq the
    // parent does not have would otherwise wait for an event that never
    // comes.
    let Ok(parent_tip) = block_on(client.get_session(parent), cancel).map(|s| s.seq()) else {
        return entries;
    };
    let through = source_seq.min(parent_tip);
    let Ok(mut parent_stream) = block_on(client.subscribe(SubscribeEvents::new(parent, 0)), cancel)
    else {
        return entries;
    };
    let mut projected = AppState::new();
    let mut folded = 0usize;
    while folded < max_events {
        if parent_stream.cursor() >= through || cancel.is_cancelled() {
            break;
        }
        let Ok(event) = parent_stream.recv() else {
            break;
        };
        projected = reduce(projected, &UiEvent::Kernel(event));
        folded += 1;
    }
    entries.extend(projected.transcript().iter().cloned());
    if let Some(err) = projected.protocol_error() {
        entries.push(TranscriptEntry::CommandError {
            text: format!("the transcript before this fork could not be fully read: {err}"),
        });
    } else if parent_stream.cursor() < through {
        entries.push(TranscriptEntry::CommandError {
            text: "the transcript before this fork was not fully read".to_owned(),
        });
    }
    entries
}

/// Fold a resumed session's replayed history into `ui` before the first
/// paint.
///
/// Replays *through a known tip* rather than until the stream goes quiet.
/// The subscription is fed by a worker thread, so `try_recv() == Ok(None)`
/// means "nothing queued yet", not "history exhausted" — stopping there
/// truncates the transcript at whatever point the worker happened to have
/// reached, which is a race, not a bound. `through` is the session's seq as
/// read immediately before subscribing, and waiting for it cannot hang: the
/// ledger is append-only (nothing deletes events — even `rewind` returns a
/// prefix *projection* without mutating the stream), so `1..=through` is
/// still there when the worker goes looking, and the durable-gap path waits
/// for a channel slot rather than declaring the consumer lagged, so none of
/// those events is dropped on the way to us.
fn replay_history(
    client: &InProcessKernelClient,
    stream: &mut EventStream,
    ui: &mut AppState,
    session_id: protocol::SessionId,
    through: u64,
    cancel: &CancellationToken,
) -> Result<(), InteractiveError> {
    let mut reconnects = 0;
    for _ in 0..MAX_REPLAYED_EVENTS {
        if stream.cursor() >= through {
            break;
        }
        cancel.check().map_err(|_| InteractiveError::Cancelled)?;
        match stream.recv() {
            Ok(event) => *ui = reduce(ui.clone(), &UiEvent::Kernel(event)),
            // Same contract as `drain_kernel_events`: a lag mid-replay is
            // resumed from its cursor, not the end of the session.
            Err(EventStreamError::Lagged { resume_cursor })
                if reconnects < MAX_LAG_RECONNECTS_PER_TICK =>
            {
                reconnects += 1;
                *stream = block_on(
                    client.subscribe(SubscribeEvents::new(session_id, resume_cursor)),
                    cancel,
                )?;
            }
            Err(err) => return Err(InteractiveError::Stream(err)),
        }
    }
    // Reconcile against the authoritative snapshot the same way `drain` does
    // once caught up — goal/agent rows come from there, not from the
    // transcript events.
    if let Ok(snapshot) = block_on(client.get_session(session_id), cancel)
        && ui.snapshot().map(|current| current.seq()).unwrap_or(0) >= snapshot.seq()
    {
        *ui = reduce(ui.clone(), &UiEvent::Snapshot(snapshot));
    }
    Ok(())
}

/// How many times one drain re-subscribes after the live channel lagged
/// before giving up on the tick. Each reconnect replays from the cursor the
/// lag reported, so nothing is lost; the bound only keeps a channel that
/// overflows faster than it can be read from spinning here.
const MAX_LAG_RECONNECTS_PER_TICK: usize = 8;

/// One tick's worth of the live subscription folded into `ui`.
///
/// The live channel is bounded (`event_ledger::subscription::DEFAULT_LIVE_
/// BOUND`, 64 events) and disconnects with a resume cursor when a burst
/// outruns the tick — a tool-heavy turn, a job's output, an autonomous run
/// — which used to end the whole interactive session as a stream error.
/// The ledger's own contract is that a subscription from the resume cursor
/// continues without duplicates or gaps, so that is what happens here.
fn drain_kernel_events(
    client: &InProcessKernelClient,
    stream: &mut EventStream,
    ui: &mut AppState,
    session_id: protocol::SessionId,
    cancel: &CancellationToken,
) -> Result<(), InteractiveError> {
    let mut reconnects = 0;
    for i in 0..MAX_EVENTS_PER_TICK {
        cancel.check().map_err(|_| InteractiveError::Cancelled)?;
        match stream.try_recv() {
            Ok(Some(event)) => {
                *ui = reduce(ui.clone(), &UiEvent::Kernel(event));
            }
            Ok(None) => break,
            Err(EventStreamError::Lagged { resume_cursor })
                if reconnects < MAX_LAG_RECONNECTS_PER_TICK =>
            {
                reconnects += 1;
                *stream = block_on(
                    client.subscribe(SubscribeEvents::new(session_id, resume_cursor)),
                    cancel,
                )?;
            }
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
/// Project the session's configured models into the frontend, so `/models`
/// shows what is actually configured rather than an empty panel.
///
/// Read through `user_config`'s own loader — the same source
/// `rapid doctor` reports from and the same one a turn resolves its model
/// through — so the panel cannot describe a different configuration than the
/// one that will run. A missing or unreadable config projects nothing, and
/// the panel says so; this is a display, and a config problem is `doctor`'s
/// job to explain.
///
/// Credentials are excluded by construction: [`tui::state::ModelRow`] has
/// nowhere to put one.
fn sync_configured_models(ui: &mut AppState) {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let source = crate::user_config::resolve_config_source(&env);
    let Ok(Some(config)) = crate::user_config::load_config(&source) else {
        return;
    };
    let active = crate::user_config::select_active_model_gated(&env)
        .ok()
        .and_then(|selection| match selection {
            crate::user_config::ModelSelection::Configured { active, .. } => {
                Some(active.profile_id)
            }
            _ => None,
        });
    let rows = model_rows(&config, active.as_deref());
    if rows.is_empty() {
        return;
    }
    *ui = reduce(ui.clone(), &UiEvent::Local(LocalUiEvent::SyncModels(rows)));
}

/// Project the project memory index into the frontend, so `/memory` shows
/// what the model is actually given.
///
/// Reuses `host::load_memory_index` — the very call a turn makes to build
/// the model's context — rather than reading `MEMORY.md` again with its own
/// bounds. A panel that showed a *different* truncation than the model
/// received would be worse than no panel: it would answer "what does the
/// model know" with something the model never saw.
fn sync_memory_index(ui: &mut AppState, root: &Path) {
    let Some(text) = crate::host::load_memory_index(root) else {
        return;
    };
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        return;
    }
    *ui = reduce(ui.clone(), &UiEvent::Local(LocalUiEvent::SyncMemory(lines)));
}

/// The `/models` rows for a loaded config, split from the reading of it so
/// the mapping is testable without depending on the machine's own
/// environment and config file.
fn model_rows(
    config: &crate::user_config::UserConfig,
    active: Option<&str>,
) -> Vec<tui::state::ModelRow> {
    config
        .models
        .entries
        .iter()
        .map(|(id, entry)| tui::state::ModelRow {
            id: id.clone(),
            provider: entry.provider.as_str().to_owned(),
            model: entry.model.clone(),
            active: active == Some(id.as_str()),
            fallback_rank: config.models.fallback.iter().position(|name| name == id),
            context_window: entry.context_window,
        })
        .collect()
}

/// The status line's session-level chrome: which model this session resolved
/// and how its permission mode answers by default.
///
/// Deliberately **not** everything the bar can show. `sandbox` stays
/// `Unknown` because sandboxing here is per *call* (`shell_exec` takes
/// `"sandbox": true`), so a session-wide "host-restricted" would tell a user
/// their commands are confined when most are not — the dangerous direction
/// for a security indicator. `context` stays `Unknown` because the compiled
/// context size is a per-turn fact that nothing projects into `AppState`;
/// showing the model's window with a `used` of zero would be wrong the
/// moment a turn ran.
fn session_status_chrome(mode: crate::permissions::PermissionMode) -> tui::StatusChrome {
    let chrome = tui::StatusChrome::default().with_policy(policy_mode_for(mode));
    match crate::user_config::select_from_process_env_gated() {
        Ok(crate::user_config::ModelSelection::Configured { active, .. }) => {
            let label = active
                .entry
                .name
                .clone()
                .unwrap_or_else(|| active.entry.model.clone());
            chrome
                .with_model(&label)
                .with_provider(active.entry.provider.as_str())
        }
        // No model configured, or a managed policy refused it: the bar keeps
        // its dash, which is what "no model resolved" honestly looks like.
        _ => chrome,
    }
}

/// How `mode` answers a tool call by default, read off
/// `PermissionLattice`'s own mode table rather than inferred from the mode's
/// name — `dontAsk` **denies** rather than allowing without prompting, which
/// a name-based guess gets backwards.
///
/// `AcceptEdits`/`Auto` allow edits and ask for everything else, which three
/// values cannot express; they report `Allow`, because overstating how
/// permissive the session is keeps a user cautious while understating it
/// would not.
fn policy_mode_for(mode: crate::permissions::PermissionMode) -> tui::PolicyMode {
    use crate::permissions::PermissionMode as Mode;
    match mode {
        Mode::Default => tui::PolicyMode::Ask,
        Mode::Plan | Mode::DontAsk => tui::PolicyMode::Deny,
        Mode::AcceptEdits | Mode::Auto | Mode::BypassPermissions => tui::PolicyMode::Allow,
    }
}

struct TuiRenderer {
    transcript: tui::Transcript,
    viewport: tui::TranscriptViewport,
    rendered_entries: usize,
    sink: RenderSink,
    /// Session-level facts the status line shows that `AppState` does not
    /// carry: which model this session resolved, and how the permission mode
    /// answers by default.
    ///
    /// Every frame used to pass `StatusChrome::default()`, so the bar read
    /// `model:-  sandbox:-  policy:-  ctx:-` for the whole session — a
    /// complete widget (labels, compaction, drop-order) fed nothing, on the
    /// one surface that is always on screen.
    chrome: tui::StatusChrome,
}

impl TuiRenderer {
    fn new(capture: bool) -> Self {
        Self {
            transcript: tui::Transcript::new(),
            viewport: tui::TranscriptViewport::new(80, 24),
            rendered_entries: 0,
            chrome: tui::StatusChrome::default(),
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
        let _ = composer.apply(tui::ComposerCommand::Insert(
            ui.composer().text().to_owned(),
        ));
        let requested_height = composer.preferred_height();
        let layout = tui::compute_screen_layout(ui, size, requested_height, modal_open);
        let composer_view = composer.render(layout.composer().width(), layout.composer().height());

        self.viewport.resize_rect(layout.transcript());

        let screen = tui::paint_screen(
            ui,
            &self.transcript,
            &self.viewport,
            composer_view.lines(),
            &self.chrome,
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

    // One-time tidy-up of the pre-unification split, from the writer only:
    // `project_ledger_path` already *reads* the legacy file where it is, so
    // nothing depends on this succeeding.
    let marker_dir = project_root.join(PROJECT_MARKER);
    let ledger_notice = adopt_legacy_ledger(&marker_dir);
    let ledger_path = project_ledger_path(&marker_dir);

    Ok(ResolvedProject {
        trust,
        executable_config_active: trust.is_trusted(),
        config,
        ledger_path,
        ledger_notice,
        root: project_root,
        user_home,
    })
}

pub(crate) fn detect_project_root(
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

/// The RapidLM home for an interactive session: an explicit
/// `InteractiveOptions::user_home` if given, otherwise the same resolution
/// every other command uses.
///
/// The environment branch used to iterate `options.env` and return on the
/// *first* of `RAPIDLM_HOME`/`HOME`/`USERPROFILE` it happened to encounter —
/// i.e. `environ` order, not a precedence — while [`user_home_from`], which
/// `rapid exec`, `rapid trust`, `rapid doctor` and `rapid mcp` all use,
/// checks `RAPIDLM_HOME` first unconditionally. With `RAPIDLM_HOME` set, a
/// TUI session and a headless command in the same project could therefore
/// resolve different homes and report different trust. It now delegates.
fn resolve_user_home(options: &InteractiveOptions) -> Result<PathBuf, InteractiveError> {
    if let Some(home) = options.user_home.as_ref() {
        return canonicalize_or_create(home);
    }
    user_home_from(&options.env).ok_or(InteractiveError::UserHomeMissing)
}

pub(crate) fn canonicalize_dir(path: &Path) -> Result<PathBuf, InteractiveError> {
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
            resume: None,
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
            Self::Usage
            | Self::UnknownSession(_)
            | Self::NotATty
            | Self::UserHomeMissing
            | Self::InvalidProjectRoot => JsonlExitCode::Usage.as_i32(),
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
            Self::UnknownSession(id) => {
                write!(f, "rapid: no session {id} in this project")
            }
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
        let alternate = ConfiguredModel::build(&active_from_doc(alternate_doc), &store_b)
            .expect("build alternate");

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
    fn the_phases_compact_override_is_resolved_and_gated_like_a_fallback_entry() {
        // `[phases] compact` was parsed and validated since the phases
        // table existed and honoured by nobody. It names the model a
        // `/compact` runs on — through the same managed gate a fallback
        // entry passes, never around it.
        let doc = r#"
[models]
default = "main"

[model.main]
provider = "anthropic"
model = "claude-sonnet-5"
base_url = "https://api.anthropic.com"

[model.cheap]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"

[phases]
compact = "cheap"
"#;
        let config_path = std::env::temp_dir().join(format!(
            "rapidlm-phases-compact-{}-{}.toml",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&config_path, doc).expect("write config");
        let env = vec![(
            crate::user_config::CONFIG_PATH_ENV.to_owned(),
            config_path.display().to_string(),
        )];
        let mut warnings = Vec::new();
        let plan = resolve_model_plan(
            &env,
            agent_runtime::reminders::ReminderFloor::Baseline,
            &mut |line| warnings.push(line.to_owned()),
        )
        .expect("plan");
        assert_eq!(plan.models[0].profile_id, "main");
        assert_eq!(
            plan.compact.as_ref().map(|m| m.profile_id.as_str()),
            Some("cheap"),
            "{warnings:?}"
        );

        // A managed allowlist that excludes the compact model's provider:
        // compaction falls back to the conversation model, with a warning.
        let policy_path = std::env::temp_dir().join(format!(
            "rapidlm-phases-compact-policy-{}-{}.toml",
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
        let gated_env = vec![
            (
                crate::user_config::CONFIG_PATH_ENV.to_owned(),
                config_path.display().to_string(),
            ),
            (
                crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
                policy_path.display().to_string(),
            ),
        ];
        let mut warnings = Vec::new();
        let plan = resolve_model_plan(
            &gated_env,
            agent_runtime::reminders::ReminderFloor::Baseline,
            &mut |line| warnings.push(line.to_owned()),
        )
        .expect("plan");
        assert_eq!(plan.compact, None, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("phases.compact") && w.contains("allowlist")),
            "{warnings:?}"
        );

        // No override, or one naming the primary: nothing routed elsewhere.
        let same = doc.replace("compact = \"cheap\"", "compact = \"main\"");
        std::fs::write(&config_path, same).expect("write config");
        let plan = resolve_model_plan(
            &env,
            agent_runtime::reminders::ReminderFloor::Baseline,
            &mut |_| {},
        )
        .expect("plan");
        assert_eq!(plan.compact, None);
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_file(&policy_path);
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
        assert!(
            gated.is_empty(),
            "candidate must be blocked by the allowlist"
        );
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
        assert_eq!(
            parse_exec_args(&unbounded).expect("parses").max_wall_time,
            None
        );
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
        let accept_edits =
            exec_permission_lattice(None, Some(crate::permissions::PermissionMode::AcceptEdits))
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
        assert!(
            cancel.is_cancelled(),
            "stays cancelled, no panic or double-fire"
        );
    }

    /// **Characterization test for a known, open gap — it asserts what the
    /// product does today, not what it should do.**
    ///
    /// In `PermissionMode::Default` — the out-of-box mode for the
    /// interactive TUI *and* headless exec — every non-read tool call is
    /// `Decision::Ask`, and `ExecTools` turns any non-`Allow` decision into
    /// a denial because no surface in this build can prompt for an approval.
    /// So a fresh `rapid` session in a trusted project can read files and
    /// nothing else: no write, no shell command.
    ///
    /// Each half of that is separately tested and separately correct
    /// (`permissions.rs` proves `Default` asks; `exec_tools.rs` proves a
    /// non-allowed decision is denied). The *composition* was untested, and
    /// could not be caught by the interactive turn suite either, because
    /// `run_interactive_turn_inner_with_backing` forces
    /// `BypassPermissions` so its tests can be hermetic.
    ///
    /// This drives the exact chain `build_interactive_turn_context` builds
    /// in production — `exec_permission_lattice(root, None)` then
    /// `ExecTools::workspace_with_permissions` — so when the approval path
    /// is built (see `newtask.md`'s "the interactive TUI cannot ask" entry
    /// and the architecture fork recorded with it), this test failing is the
    /// intended signal that the gap closed, and it should be rewritten to
    /// assert the new behavior.
    #[test]
    fn default_mode_denies_every_write_because_nothing_can_prompt_for_approval() {
        use agent_runtime::ToolDriver;

        let dir = std::env::temp_dir().join(format!(
            "rapidlm-default-mode-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let root = fs::canonicalize(&dir).expect("canonicalize");

        // Exactly what the interactive turn builds, with production's own
        // `forced_mode: None`.
        let lattice = exec_permission_lattice(Some(&root), None).expect("lattice");
        // A developer machine with RAPIDLM_PERMISSION_MODE exported would
        // otherwise make this assert something else entirely.
        assert_eq!(
            lattice.mode(),
            crate::permissions::PermissionMode::Default,
            "this test characterizes the *default* mode; unset RAPIDLM_PERMISSION_MODE to run it"
        );
        let mut tools = crate::exec_tools::ExecTools::workspace_with_permissions(&root, lattice)
            .expect("workspace tools");

        let call = agent_runtime::ProposedToolCall::new(
            "c1",
            crate::exec_tools::WORKSPACE_WRITE_TOOL,
            r#"{"path":"note.md","content":"hello"}"#,
        )
        .expect("call");
        let cancel = agent_runtime::CancellationToken::new();
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            agent_runtime::ToolStepResult::Denied { detail, .. } => {
                let detail = detail.expect("a denial names its reason");
                // The message must at least be *true* and actionable while
                // the gap is open: it used to say "headless exec cannot
                // ask", which is false in the interactive TUI, where this
                // same denial is what a user actually hits.
                assert!(
                    !detail.contains("headless exec cannot ask"),
                    "the denial still claims to be headless-only: {detail}"
                );
                assert!(
                    detail.contains("permissions.allow"),
                    "the denial must name a way forward: {detail}"
                );
            }
            other => panic!(
                "default mode is expected to deny a write today; if this now succeeds the \
approval gap has been closed and this characterization test should be rewritten: {other:?}"
            ),
        }
        assert!(
            !root.join("note.md").exists(),
            "a denied write must not touch the workspace"
        );
        let _ = fs::remove_dir_all(&dir);
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
    fn project_integrations_expose_the_merged_capped_mcp_view_the_turn_registers() {
        // The turn path hands `integrations.mcp.configs()` straight to
        // `register_mcp_servers`, so this is the exact list that becomes
        // child processes. It used to be a per-file `extend`, which meant a
        // name defined in both files spawned twice (only the first
        // reachable) and the server cap applied once per file.
        let dir = std::env::temp_dir().join(format!(
            "project-integrations-mcp-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".rapidlm")).expect("rapidlm dir");
        std::fs::create_dir_all(dir.join(".claude")).expect("claude dir");
        std::fs::write(
            dir.join(".rapidlm/settings.json"),
            r#"{"mcpServers": {"shared": {"command": "native"}, "only-native": {"command": "a"}}}"#,
        )
        .expect("rapidlm settings");
        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"mcpServers": {"shared": {"command": "compat"},
                                "only-compat": {"command": "b"},
                                "broken": {"url": "https://example.com"}}}"#,
        )
        .expect("claude settings");

        let integrations = load_project_integrations(&dir);
        let names: Vec<String> = integrations
            .mcp
            .configs()
            .into_iter()
            .map(|config| config.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "only-native".to_owned(),
                "shared".to_owned(),
                "only-compat".to_owned()
            ],
            "one entry per name, first file winning"
        );
        assert_eq!(
            integrations
                .mcp
                .get("shared")
                .expect("shared")
                .config
                .command,
            "native",
            "the earlier settings file wins a name collision"
        );
        // And the entries that will not run are reported rather than dropped.
        let rejected: Vec<&str> = integrations
            .mcp
            .rejections()
            .iter()
            .map(|rejection| rejection.name.as_str())
            .collect();
        assert_eq!(rejected, vec!["broken", "shared"]);

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
                resume: None,
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

    /// Rows of a captured frame.
    ///
    /// The renderer positions the cursor per row (`ESC [ <n> ; 1 H`) rather
    /// than emitting newlines, so `str::lines()` on captured output yields
    /// the *whole frame* as a single line — which quietly turns any
    /// per-line assertion into a frame-wide `contains`.
    fn painted_rows(painted: &str) -> Vec<String> {
        painted
            .split('\u{1b}')
            .filter_map(|chunk| {
                // Strictly `[ <digits> ; <digits> H`; anything else (`[2J`,
                // `[?25l`, …) is not a row start. A looser match returned
                // fragments of other escapes as if they were rows.
                let rest = chunk.strip_prefix('[')?;
                let (row, rest) = rest.split_once(';')?;
                if row.is_empty() || !row.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let (col, body) = rest.split_once('H')?;
                if col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Some(body.trim_end().to_owned())
            })
            .filter(|row| !row.is_empty())
            .collect()
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
    fn cli_usage_lists_exactly_the_dispatched_subcommands() {
        // Both directions. A command the binary runs but never mentions is
        // undiscoverable; a command `--help` names but cannot run is a lie
        // that reads, to a user, exactly like a typo. `CLI_USAGE` had both
        // faults at once: `trust`, `scan`, `insights` and `release-manifest`
        // were dispatched but undocumented in the subcommand table, and
        // eighteen families were advertised with no dispatch arm at all.
        // Every indented line under `Commands:` must be a real command
        // line — asserted by *shape*, not by filtering. A `filter_map` that
        // silently drops non-matching lines would let any prose line
        // (`  see also: rapid daemon`, or an entry written `rapid
        // interactive`) sit in the block unchecked.
        let block: Vec<&str> = CLI_USAGE
            .lines()
            .skip_while(|line| !line.starts_with("Commands:"))
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .collect();
        assert!(
            block.len() > 15,
            "the Commands block did not parse: {block:?}"
        );
        let mut advertised: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut previous_was_an_entry = false;
        for line in &block {
            // An entry whose invocation reaches the summary column carries
            // its summary on a following, fully-indented line. That is the
            // *only* line shape allowed besides an entry, and it may never
            // start the block — so prose still cannot hide here.
            if !line.starts_with("  rapid ") {
                assert!(
                    previous_was_an_entry
                        && line.starts_with(&" ".repeat(SUBCOMMAND_SUMMARY_COLUMN))
                        && !line.trim().is_empty(),
                    "a line under `Commands:` is neither a command nor the wrapped \
summary of the one above it: {line:?}"
                );
                previous_was_an_entry = false;
                continue;
            }
            previous_was_an_entry = true;
            let rest = line.strip_prefix("  rapid ").unwrap_or_else(|| {
                panic!("every line under `Commands:` must be `  rapid <name> ...`: {line:?}")
            });
            let word = rest
                .split_whitespace()
                .next()
                .unwrap_or_else(|| panic!("no command name on: {line:?}"));
            assert!(
                !word.starts_with('<') && !word.starts_with('-'),
                "the first word after `rapid` must be the subcommand name: {line:?}"
            );
            advertised.insert(word.to_owned());
        }
        let dispatched: std::collections::BTreeSet<String> = SUBCOMMANDS
            .iter()
            .map(|entry| entry.name.to_owned())
            .collect();
        let undocumented: Vec<&String> = dispatched.difference(&advertised).collect();
        assert!(
            undocumented.is_empty(),
            "dispatched but missing from `rapid --help`: {undocumented:?}"
        );
        let unrunnable: Vec<&String> = advertised.difference(&dispatched).collect();
        assert!(
            unrunnable.is_empty(),
            "advertised by `rapid --help` but not dispatched: {unrunnable:?}"
        );
    }

    #[test]
    fn no_subcommand_summary_is_pushed_past_the_terminal_s_width() {
        // The rule is about *placement*, not about entries: a summary must
        // never be what makes a line too wide, because a wrapped summary
        // breaks the alignment of every line after it. An invocation that is
        // itself long (`rapid goal`'s eleven alternatives) is allowed to be
        // as long as its operands require — there is nothing to trim but the
        // truth — and its summary moves to the next line, which this asserts
        // by construction rather than by exempting a named command.
        const WIDTH: usize = 80;
        for entry in SUBCOMMANDS {
            let rendered = render_subcommand_line(entry);
            let lines: Vec<&str> = rendered.lines().collect();
            for line in &lines {
                if line.trim_start().starts_with("rapid ") && lines.len() == 2 {
                    // The invocation-only line of a wrapped entry.
                    continue;
                }
                assert!(
                    line.chars().count() <= WIDTH,
                    "`{}` renders a {}-column line, which wraps and breaks the \
alignment below it: {line:?}",
                    entry.name,
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn every_subcommand_line_states_what_that_command_takes() {
        // The defect this table's `operands` field exists for: `rapid --help`
        // advertised `rapid inspect-export <session>` while the parser
        // required a second positional, so the *documented* invocation could
        // only fail — with an error telling the reader to consult the help
        // they had just followed. Help, `rapid <name> --help` and `rapid man`
        // all render this one field now.
        let usage = CLI_USAGE.as_str();
        let line = usage
            .lines()
            .find(|line| line.trim_start().starts_with("rapid inspect-export"))
            .expect("an inspect-export line");
        assert!(
            line.contains("<session>") && line.contains("<out-path>"),
            "the help must name both positionals the parser requires: {line}"
        );

        // And every entry's operand text actually reaches the help, so a new
        // command cannot be added with operands that are never shown.
        for entry in SUBCOMMANDS {
            if entry.operands.is_empty() {
                continue;
            }
            assert!(
                usage.contains(&format!("rapid {} {}", entry.name, entry.operands)),
                "`rapid {}`'s operands never reach the help",
                entry.name
            );
        }
    }

    #[test]
    fn cli_usage_lists_exactly_the_goal_subcommands() {
        // Same defect class as the top-level table, one level down, and it
        // had a live instance: `CLI_USAGE` advertised `rapid goal … budget
        // …`, which no arm has ever matched, while omitting `replace` and
        // `complete`, which do.
        let line = CLI_USAGE
            .lines()
            .find(|line| line.trim_start().starts_with("rapid goal "))
            .expect("a `rapid goal` line");
        let advertised: std::collections::BTreeSet<&str> = line
            .split_whitespace()
            .nth(2)
            .expect("the alternatives")
            .split('|')
            .collect();
        let dispatched: std::collections::BTreeSet<&str> =
            GOAL_SUBCOMMANDS.iter().copied().collect();
        assert_eq!(
            advertised, dispatched,
            "`rapid goal`'s advertised subcommands and its dispatched ones disagree"
        );
    }

    #[test]
    fn the_goal_subcommand_list_is_the_set_the_dispatcher_really_carries() {
        // `rapid goal budget` was advertised by `CLI_USAGE` and matched no
        // arm, while `replace` and `complete` were dispatched and never
        // advertised. `cli_usage_lists_exactly_the_goal_subcommands` ties
        // the usage line to this list; this ties the list to the three
        // specific facts that were wrong.
        //
        // Deliberately does *not* call `run_goal_command`: an unknown name
        // and a real name with missing operands both return
        // `InteractiveError::Usage`, so such a call would pass whether or
        // not the guard exists — a vacuous assertion. The guard's own input
        // is this list, which is what is checked here.
        let unique: std::collections::BTreeSet<&&str> = GOAL_SUBCOMMANDS.iter().collect();
        assert_eq!(
            unique.len(),
            GOAL_SUBCOMMANDS.len(),
            "duplicate goal subcommand"
        );
        assert!(
            !GOAL_SUBCOMMANDS.contains(&"budget"),
            "`budget` has no arm in run_goal_command"
        );
        assert!(GOAL_SUBCOMMANDS.contains(&"replace"));
        assert!(GOAL_SUBCOMMANDS.contains(&"complete"));
    }

    #[test]
    fn every_emitted_completion_script_is_well_formed_for_its_shell() {
        // All three emitters were broken and nothing checked them: bash put
        // the command name before `-W` (a fish flag order), zsh called
        // `compdef` before defining the function, and fish interpolated
        // summaries containing an apostrophe straight into single quotes,
        // unbalancing the quoting and aborting the whole file.
        let bash = crate::p9_commands::completions_script("bash").expect("bash");
        assert!(bash.trim_end().ends_with(" rapid"), "{bash}");
        assert!(bash.contains("-W \""), "{bash}");

        let zsh = crate::p9_commands::completions_script("zsh").expect("zsh");
        let define = zsh.find("_rapid()").expect("the function is defined");
        let compdef = zsh.find("compdef").expect("compdef is called");
        assert!(
            define < compdef,
            "compdef must come after the definition:\n{zsh}"
        );

        let fish = crate::p9_commands::completions_script("fish").expect("fish");
        for line in fish.lines() {
            // Count single quotes that are not backslash-escaped.
            let mut quotes = 0usize;
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        let _ = chars.next();
                    }
                    '\'' => quotes += 1,
                    _ => {}
                }
            }
            assert_eq!(quotes % 2, 0, "unbalanced quoting in fish line: {line}");
        }
        for entry in SUBCOMMANDS {
            assert!(
                fish.contains(entry.name),
                "fish completions omit {}",
                entry.name
            );
        }
        assert!(crate::p9_commands::completions_script("tcsh").is_none());
    }

    #[test]
    fn getting_started_lists_exactly_the_exit_codes() {
        // `docs/getting-started.md` tells a script author what each exit
        // code means. The numbers and the wording both come from
        // `JsonlExitCode::ALL`, and this is the check that keeps them there:
        // a code added, removed or renumbered in the binary fails here until
        // the table says so too.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/getting-started.md");
        let doc =
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let start = doc
            .find("<!-- exit-codes:")
            .expect("the exit-code table marker");
        let end = doc[start..]
            .find("\n## ")
            .map(|offset| start + offset)
            .expect("a heading follows the table");
        let table = &doc[start..end];
        let mut documented: Vec<(i32, String)> = Vec::new();
        for line in table.lines() {
            let Some(rest) = line.strip_prefix("| `") else {
                continue;
            };
            let Some((number, meaning)) = rest.split_once("` | ") else {
                continue;
            };
            let meaning = meaning.trim_end_matches(" |").trim();
            documented.push((
                number
                    .parse()
                    .unwrap_or_else(|_| panic!("exit code `{number}` is not a number")),
                meaning.to_owned(),
            ));
        }
        let shipped: Vec<(i32, String)> = crate::headless::jsonl::JsonlExitCode::ALL
            .iter()
            .map(|(code, meaning)| (code.as_i32(), (*meaning).to_owned()))
            .collect();
        assert_eq!(
            documented, shipped,
            "docs/getting-started.md's exit-code table must match JsonlExitCode::ALL, in order"
        );
    }

    #[test]
    fn the_reference_doc_lists_exactly_the_dispatched_subcommands() {
        // `docs/reference/cli-command-reference.md` is deliberately a
        // *target surface* document, so most of its table is roadmap. The
        // one paragraph that claims to describe what ships must actually
        // do so — it asserted it was "generated from one table in source",
        // which it is not; it is hand-typed markdown. This is the check
        // that makes the claim true.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/reference/cli-command-reference.md");
        let doc =
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let start = doc
            .find("> **What the binary actually dispatches today**")
            .expect("the shipped-commands paragraph");
        let end = doc[start..]
            .find("\n\n")
            .map(|offset| start + offset)
            .expect("the paragraph ends");
        let paragraph = &doc[start..end];
        let mut listed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut rest = paragraph;
        while let Some(open) = rest.find('`') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('`') else { break };
            let word = &rest[..close];
            rest = &rest[close + 1..];
            // The paragraph also cites `interactive::SUBCOMMANDS` and the
            // literal `rapid --help` output shape; only bare names count.
            if word
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            {
                listed.insert(word.to_owned());
            }
        }
        let dispatched: std::collections::BTreeSet<String> = SUBCOMMANDS
            .iter()
            .map(|entry| entry.name.to_owned())
            .collect();
        assert_eq!(
            listed, dispatched,
            "the reference doc's shipped-commands paragraph disagrees with SUBCOMMANDS"
        );
    }

    #[test]
    fn every_dispatched_subcommand_carries_a_summary_and_a_unique_name() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in SUBCOMMANDS {
            assert!(
                seen.insert(entry.name),
                "duplicate subcommand name: {}",
                entry.name
            );
            assert!(!entry.name.is_empty());
            assert!(
                entry.summary.len() > 10,
                "`{}` has no real summary; it is printed by `rapid --help`, \
`rapid completions fish` and `rapid man`",
                entry.name
            );
        }
    }

    #[test]
    fn an_unknown_subcommand_is_named_rather_than_answered_with_a_bare_usage_dump() {
        // Asserted on the *message*, not on `run_subcommand`'s return value:
        // that is `Err(InteractiveError::Usage)` both before and after this
        // change, so a test that only called it would pass with the message
        // deleted.
        let text = unknown_subcommand_text("daemon");
        assert!(text.contains("daemon"), "{text}");
        assert!(text.contains("unknown subcommand"), "{text}");
        assert_ne!(
            text,
            InteractiveError::Usage.to_string(),
            "the point is that it differs from the bare usage line a typo used to get"
        );
    }

    #[test]
    fn a_leading_flag_is_not_reported_as_an_unknown_subcommand() {
        // `classify_launch` looks past leading flags to decide this is a
        // subcommand launch at all, so `run_subcommand` has to as well.
        // Reading `args.first()` blindly told a user that `--help` and
        // `--jsonl` were unknown subcommands.
        assert_eq!(
            classify_launch(&["--jsonl", "man"]),
            LaunchMode::Subcommand,
            "precondition: a leading flag still selects subcommand mode"
        );
        assert!(matches!(
            run_subcommand(&["--jsonl".to_owned(), "man".to_owned()]),
            Ok(0)
        ));
        // A flag-only argv has no subcommand at all, and must not name one.
        assert!(matches!(
            run_subcommand(&["--jsonl".to_owned()]),
            Err(InteractiveError::Usage)
        ));
    }

    #[test]
    fn every_subcommand_answers_help_because_the_usage_text_promises_it_does() {
        // `CLI_USAGE` says every command answers `--help`. Thirteen of them
        // used to answer with ``usage: see `rapid --help` `` and exit 2 — a
        // literal loop — `playbook-compile` tried to open a file named
        // `--help`, and `sessions`/`mcp-tools`/`man` ignored the flag and
        // ran. Commands that handle it themselves are exercised by their own
        // suites; this covers the centrally-answered ones.
        for entry in SUBCOMMANDS {
            if entry.own_help {
                continue;
            }
            let result = run_subcommand(&[entry.name.to_owned(), "--help".to_owned()]);
            assert!(
                matches!(result, Ok(0)),
                "`rapid {} --help` did not answer: {result:?}",
                entry.name
            );
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
    fn the_second_turn_sees_what_the_first_turn_said() {
        // A session is a conversation. Every turn used to run on its prompt
        // alone — the transcript on screen was the user's memory, not the
        // model's — so "now fix the tests" on turn two meant nothing to it.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "call the project Nightjar from now on",
            ScriptedModel::terminal("Noted: the project is Nightjar."),
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "what did I call the project?",
            ScriptedModel::terminal("Nightjar.").capturing_blocks(seen.clone()),
        );
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        let turns: Vec<&(String, String)> = seen
            .iter()
            .filter(|(locator, _)| locator.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .collect();
        assert_eq!(
            turns.len(),
            1,
            "exactly the one earlier turn is carried: {:?}",
            seen.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>()
        );
        let text = &turns[0].1;
        assert!(
            text.contains("call the project Nightjar from now on"),
            "{text}"
        );
        assert!(text.contains("Noted: the project is Nightjar."), "{text}");
        assert!(
            !text.contains("what did I call the project?"),
            "the turn being run is the goal, not history: {text}"
        );
        assert!(
            seen.iter()
                .any(|(_, text)| text.contains("what did I call the project?")),
            "and the current prompt is still the goal"
        );
    }

    #[test]
    fn a_rewound_session_remembers_the_turns_before_the_rewind_and_not_after() {
        // `/rewind <seq>` forks at that seq and switches to the child. The
        // child's own ledger starts at `session.forked`; what was said
        // before lives in the parent, and the model on the child must have
        // it — up to the rewind point only.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("first: name it Nightjar", ScriptedModel::terminal("Named."));
        let cancel = CancellationToken::new();
        let after_first =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        session.run_turn(
            "second: rename it Kestrel",
            ScriptedModel::terminal("Renamed."),
        );

        // Rewind to just after the first turn.
        let child = block_on(
            session.client.fork_session(ForkSession::new(
                session.session_id,
                after_first.seq(),
                session.actor.clone(),
                TraceId::new(),
            )),
            &cancel,
        )
        .expect("fork");
        session.session_id = child.id();

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "what is it called?",
            ScriptedModel::terminal("Nightjar.").capturing_blocks(seen.clone()),
        );
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        let turns: Vec<&str> = seen
            .iter()
            .filter(|(locator, _)| locator.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert!(turns[0].contains("name it Nightjar"), "{}", turns[0]);
        assert!(
            !seen.iter().any(|(_, text)| text.contains("Kestrel")),
            "the turn after the rewind point is gone"
        );
    }

    #[test]
    fn a_failed_and_an_interrupted_turn_are_carried_as_what_they_were() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("try something", ScriptedModel::failing());
        session.run_turn("and then this worked", ScriptedModel::terminal("Done."));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "recap",
            ScriptedModel::terminal("Recap.").capturing_blocks(seen.clone()),
        );
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        let turns: Vec<&str> = seen
            .iter()
            .filter(|(locator, _)| locator.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert!(
            turns[0].contains("try something") && turns[0].contains("the turn failed"),
            "{}",
            turns[0]
        );
        assert!(
            turns[1].contains("and then this worked") && turns[1].contains("Done."),
            "{}",
            turns[1]
        );
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("unknown command") && painted.contains("/help"),
            "{painted}"
        );
    }

    #[test]
    fn invalid_slash_command_arguments_show_specific_usage_not_a_generic_failure_or_the_full_catalog()
     {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/fork extra-argument".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("invalid arguments on a known command must not end the session either");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
            InteractiveInput::Resize {
                width: 80,
                height: 60,
            },
            InteractiveInput::Submit("/help".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("goal started: ship the thing"),
            "{painted}"
        );

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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("goal pause: ok"), "{painted}");
        assert!(painted.contains("goal resume: ok"), "{painted}");

        let goal_path = env.project.join(PROJECT_MARKER).join(GOAL_FILE);
        let host = GoalHost::load(&goal_path)
            .expect("load")
            .expect("goal exists");
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("goal cancel: ok"), "{painted}");
        // `rendered_output` is every frame this session ever painted,
        // concatenated (the renderer's own capture buffer has no per-frame
        // boundary) — "goal started: cancel me please" legitimately appears
        // in an *earlier* frame and always will. What must not survive into
        // the *last* painted frame is the Goals panel still showing that
        // goal — split on this renderer's own screen-clear sequence
        // (`crossterm::terminal::Clear(ClearType::All)`, emitted once per
        // `TuiRenderer::render` call) and check only the final one.
        let last_frame = painted
            .rsplit("\u{1b}[2J")
            .next()
            .expect("at least one frame");
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
            vec![
                agent_runtime::EvidenceRequirement::new("c1", vec!["test".to_owned()])
                    .expect("req"),
            ],
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
                settle_last_turn(loop_state);
                return;
            }
        }
        panic!("autonomous goal loop did not reach a terminal state in time");
    }

    /// Once the loop has stopped, keep draining until the last turn's
    /// terminal entry has reached the transcript. The loop decides to stop
    /// on what it has seen so far — a `tool.context_required`, say — and
    /// the turn's own `turn.completed` is delivered by the subscription's
    /// worker thread, which can still be behind at that moment (CI's Linux
    /// runner, where an assertion on the answer text ran ahead of it).
    /// Keyed to the ledger's own record, not to a sleep.
    fn settle_last_turn(loop_state: &mut SessionLoop) {
        for _ in 0..300 {
            loop_state.drain().expect("drain");
            let transcript = loop_state.ui.transcript();
            let last_user = transcript
                .iter()
                .rposition(|entry| matches!(entry, TranscriptEntry::User { .. }));
            let terminal_after = |from: usize| {
                transcript[from..].iter().any(|entry| {
                    matches!(
                        entry,
                        TranscriptEntry::Assistant { .. }
                            | TranscriptEntry::TurnFailed { .. }
                            | TranscriptEntry::TurnInterrupted
                    )
                })
            };
            match last_user {
                None => return,
                Some(at) if terminal_after(at) => return,
                Some(_) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the last turn's terminal entry never reached the transcript: {:?}",
            loop_state.ui.transcript()
        );
    }

    fn scripted_backing_queue(models: Vec<ScriptedModel>) -> ScriptedBackingQueue {
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
            user_home: &session.user_home,
            trusted: true,
            turn_in_flight,
            jobs: session.jobs.clone(),
            renderer,
            autonomous: None,
            compaction: None,
            shared: session.shared.clone(),
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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

        assert!(
            loop_state.autonomous.is_none(),
            "pausing must stop the autonomous loop"
        );
        let host = GoalHost::load(&session.goal_path())
            .expect("load")
            .expect("goal exists");
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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
        assert!(
            loop_state.autonomous.is_some(),
            "the first iteration must be submitted"
        );
        drive_autonomous_goal(&mut loop_state);

        assert!(
            loop_state.autonomous.is_none(),
            "the loop must have stopped once the turn budget was exhausted"
        );
        let host = GoalHost::load(&session.goal_path())
            .expect("load")
            .expect("goal exists");
        let snapshot = host.snapshot().expect("snapshot");
        assert_eq!(snapshot.state(), GoalState::Blocked);
        assert_eq!(
            snapshot.stop_reason(),
            Some(agent_runtime::GoalStopReason::BudgetExhausted)
        );
        let usage = snapshot.usage();
        assert_eq!(
            usage.turns(),
            2,
            "exactly two real iterations, never a third"
        );
        assert_eq!(
            usage.tokens(),
            110,
            "50 + 60 — each iteration's real tokens accrued exactly once"
        );
        assert_eq!(
            usage.cost(),
            500,
            "200 + 300 — each iteration's real cost accrued exactly once"
        );
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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
        assert_eq!(
            prompts.len(),
            1,
            "exactly one autonomous iteration must have called step()"
        );
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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

        assert!(
            loop_state.autonomous.is_none(),
            "the loop must stop, not keep guessing"
        );
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
        let host = GoalHost::load(&session.goal_path())
            .expect("load")
            .expect("goal exists");
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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

        assert!(
            loop_state.autonomous.is_none(),
            "a failed turn must stop the loop"
        );
        let host = GoalHost::load(&session.goal_path())
            .expect("load")
            .expect("goal exists");
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
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
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
        inputs.extend((0..80).map(|_| InteractiveInput::Resize {
            width: 80,
            height: 24,
        }));
        inputs.push(InteractiveInput::Submit("/goal stop".to_owned()));
        inputs.push(InteractiveInput::Submit("/quit".to_owned()));
        let report = run_interactive(env.options_capturing_render(inputs)).expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
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
        inputs.extend((0..80).map(|_| InteractiveInput::Resize {
            width: 80,
            height: 24,
        }));
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
            store
                .get(&identity, &CancellationToken::new())
                .expect("get"),
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
            store
                .get(&identity, &CancellationToken::new())
                .expect("get"),
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
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("not available"), "{painted}");
        // Not just "MCP": the message must send the user to the command
        // that can actually do it, which now exists.
        assert!(painted.contains("rapid mcp add"), "{painted}");
    }

    #[test]
    fn an_inspector_with_no_tui_route_says_something_instead_of_silently_doing_nothing() {
        // Eleven of the seventeen inspectors have no route. Their dispatch
        // arm used to be an empty `if let Some(route)`, so each of these
        // commands parsed, dispatched, and produced no panel, no output and
        // no error — worse than the `KernelAction` path beside it, which has
        // named its specific gap since it existed.
        let _lock = lock_terminal();
        for command in [
            "/sandbox doctor",
            "/model list",
            "/plugins list",
            "/policy explain",
            "/knowledge list",
            "/playbook list",
            "/trace show",
            "/insights show",
            "/computer status",
            // `/mcp list` and `/permissions` were in this list too, until
            // they were given real backends. They are now covered by
            // `mcp_list_renders_the_real_project_report_inline` and
            // `bare_permissions_renders_the_real_grant_list_not_a_no_panel_note`,
            // which assert real content rather than an honest refusal.
        ] {
            let env = TempEnv::create();
            let report = run_interactive(env.options_capturing_render(vec![
                InteractiveInput::Submit(command.to_owned()),
                InteractiveInput::Submit("/quit".to_owned()),
            ]))
            .expect("run");
            assert_eq!(report.outcome, InteractiveOutcome::Quit);
            let painted = report
                .rendered_output
                .expect("capture_render was requested");
            assert!(
                painted.contains("not available"),
                "`{command}` produced no output at all:\n{painted}"
            );
        }
    }

    #[test]
    fn every_unrouted_inspector_message_names_a_specific_gap_not_a_generic_one() {
        // The value of these messages is that they differ. A table of
        // identical "not available" strings would satisfy the test above
        // while telling a user nothing.
        // `Inspector::Mcp` and `Inspector::Permissions` are absent: both are
        // handled with a real report before `unrouted_inspector_text` is
        // reached, so their entries there are unreachable placeholders.
        let messages: Vec<String> = [
            Inspector::Knowledge,
            Inspector::Playbook,
            Inspector::Trace,
            Inspector::Insights,
            Inspector::Models,
            Inspector::Plugins,
            Inspector::Policy,
            Inspector::Sandbox,
            Inspector::Computer,
        ]
        .iter()
        .map(unrouted_inspector_text)
        .collect();
        let unique: std::collections::BTreeSet<&String> = messages.iter().collect();
        assert_eq!(
            unique.len(),
            messages.len(),
            "unrouted inspector messages collapsed into duplicates: {messages:?}"
        );
        for message in &messages {
            assert!(message.starts_with("not available: "), "{message}");
            assert!(
                message.len() > "not available: ".len() + 20,
                "message is too short to name a real gap: {message}"
            );
        }
    }

    #[test]
    fn every_command_an_unrouted_inspector_message_names_actually_exists() {
        // These messages send a user somewhere else. A message naming a
        // command this binary does not dispatch would be worse than saying
        // nothing — it is the same class of untruth the whole pass exists to
        // remove.
        let dispatched: Vec<&str> = SUBCOMMANDS.iter().map(|entry| entry.name).collect();
        let all = [
            Inspector::Knowledge,
            Inspector::Playbook,
            Inspector::Trace,
            Inspector::Insights,
            Inspector::Models,
            Inspector::Plugins,
            Inspector::Policy,
            Inspector::Sandbox,
            Inspector::Computer,
        ]
        .iter()
        .map(unrouted_inspector_text)
        .collect::<Vec<_>>()
        .join("\n");

        let mut named = 0usize;
        let mut rest = all.as_str();
        while let Some(start) = rest.find("`rapid ") {
            rest = &rest[start + "`rapid ".len()..];
            let end = rest.find('`').expect("an opened backtick is closed");
            let subcommand = rest[..end]
                .split_whitespace()
                .next()
                .expect("a named command is not empty");
            assert!(
                dispatched.contains(&subcommand),
                "`rapid {subcommand}` is named as the way to do this but is not a dispatched \
subcommand"
            );
            named += 1;
            rest = &rest[end..];
        }
        assert!(
            named >= 4,
            "expected several messages to point at a real command"
        );
    }

    #[test]
    fn a_resumed_session_rebuilds_its_transcript_from_the_durable_ledger() {
        // The point of resume: the work is still there. Everything this
        // needs already existed and was unwired — `get_session` for the
        // projection and `subscribe(id, 0)`'s replay-then-tail for the
        // history — so this drives the real `run_interactive` twice against
        // the same project and asserts the second run shows the first run's
        // transcript.
        let _lock = lock_terminal();
        let env = TempEnv::create();

        // Real work, through the real turn loop: a scripted turn writes a
        // file and answers, producing the kernel events a transcript is made
        // of. Deliberately *not* a local slash command — `/permissions`,
        // `/help` and friends render `TranscriptEntry::CommandOutput` from a
        // `LocalUiEvent`, which never reaches the ledger and so cannot come
        // back; only kernel events are durable, and that is the distinction
        // this test exists to hold.
        let session_id = {
            let mut session = ScriptedSession::create(&env);
            session.run_turn(
                "write a note",
                ScriptedModel::write_then_answer("resumed.md", "hi", "the earlier answer"),
            );
            session.session_id
        };

        let mut options =
            env.options_capturing_render(vec![InteractiveInput::Submit("/quit".to_owned())]);
        options.resume = Some(session_id);
        let resumed = run_interactive(options).expect("resumed session");

        assert_eq!(
            resumed.session_id,
            Some(session_id),
            "a resumed run must stay on the session it was asked for, not create a new one"
        );
        let painted = resumed
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("the earlier answer"),
            "the resumed session must show the earlier run's assistant output:\n{painted}"
        );
        assert!(
            painted.contains("write a note"),
            "and the message that produced it:\n{painted}"
        );
    }

    #[test]
    fn a_resumed_session_does_not_replay_local_command_output() {
        // The honest limit of resume, pinned so it is a documented property
        // rather than a surprise: a slash command's own output is a
        // `LocalUiEvent` and never reaches the durable ledger, so it does
        // not come back. Only kernel events do.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let first = run_interactive(env.options(vec![
            InteractiveInput::Submit("/permissions allow workspace_write".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("first session");
        let session = first.session_id.expect("session id");

        let mut options =
            env.options_capturing_render(vec![InteractiveInput::Submit("/quit".to_owned())]);
        options.resume = Some(session);
        let resumed = run_interactive(options).expect("resumed");
        let painted = resumed
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            !painted.contains("allow=workspace_write"),
            "local command output is session-local by design and must not appear to persist:\n{painted}"
        );
        // The grant itself is durable — it is the *transcript line* that is not.
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        assert!(!persisted_grants_for(&canonical, &env.user_home).is_empty());
    }

    #[test]
    fn resuming_does_not_create_a_new_session() {
        // The ledger is the product's memory; a "resume" that quietly
        // started a fresh session would look like it worked and lose
        // everything.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let first =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("first session");
        let session = first.session_id.expect("session id");

        let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
        let sessions_after_first = recorded_sessions(&ledger_path);
        assert_eq!(sessions_after_first, 1);

        let mut options = env.options(vec![InteractiveInput::Submit("/quit".to_owned())]);
        options.resume = Some(session);
        run_interactive(options).expect("resumed");

        assert_eq!(
            recorded_sessions(&ledger_path),
            sessions_after_first,
            "resuming must not add a session to the ledger"
        );
    }

    #[test]
    fn resuming_an_unknown_session_fails_rather_than_starting_a_fresh_one() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        // Make the ledger exist without the session being in it.
        run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
            .expect("first session");

        let mut options = env.options(vec![InteractiveInput::Submit("/quit".to_owned())]);
        options.resume = Some("01234567-89ab-7cde-89ab-0123456789ab".parse().expect("id"));
        let err = run_interactive(options)
            .expect_err("an unknown session must be an error, never a silent new session");
        assert!(
            matches!(err, InteractiveError::UnknownSession(_)),
            "and must say so in its own terms rather than leaking a protocol error: {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("no session") && rendered.contains("0123456789ab"),
            "the message must name the problem and the id asked for: {rendered}"
        );
    }

    #[test]
    fn a_corrupt_ledger_row_is_neither_offered_nor_printed_verbatim() {
        // `.rapidlm/ledger.sqlite` belongs to the project, and a project is
        // only as trustworthy as its trust record says. Two properties: a row
        // whose id `rapid resume` could not accept is not offered as one to
        // try, and nothing from the row reaches stderr as an escape sequence.
        use event_ledger::ledger::SessionSummary;

        let good = "01a08600-0000-7000-8000-0123456789ab";
        let sessions = vec![
            SessionSummary {
                session_id: good.to_owned(),
                last_seq: 3,
                first_seen: "2026-09-09T10:00:00.000Z".to_owned(),
                last_activity: "2026-09-09T10:00:00.000Z".to_owned(),
            },
            SessionSummary {
                // Not an id at all, and carrying a cursor-moving escape.
                session_id: "\u{1b}[2Jnot-an-id".to_owned(),
                last_seq: 9,
                first_seen: "2026-09-09T11:00:00.000Z".to_owned(),
                // Newest, so it sorts first and would print first.
                last_activity: "2026-09-09T12:00:00.000Z".to_owned(),
            },
        ];
        let hint = hint_lines(sessions).expect("the good row is still offered");
        assert!(hint.contains(good), "the usable id must be offered: {hint}");
        assert!(
            !hint.contains("not-an-id"),
            "an id resume could not accept must not be offered: {hint}"
        );
        assert!(
            !hint.contains('\u{1b}'),
            "no escape sequence may reach the terminal: {hint:?}"
        );
    }

    #[test]
    fn a_stored_timestamp_cannot_smuggle_an_escape_sequence_to_the_terminal() {
        use event_ledger::ledger::SessionSummary;

        let hint = hint_lines(vec![SessionSummary {
            session_id: "01a08600-0000-7000-8000-0123456789ab".to_owned(),
            last_seq: 1,
            first_seen: "2026-09-09T10:00:00.000Z".to_owned(),
            last_activity: "2026\u{1b}[31m-09-09".to_owned(),
        }])
        .expect("a usable row");
        assert!(
            !hint.contains('\u{1b}'),
            "the timestamp is stored data too: {hint:?}"
        );
    }

    #[test]
    fn a_corrupt_newest_row_does_not_hide_a_resumable_older_one() {
        // Bare `rapid resume` takes the maximum of what it can *use*. Taking
        // the newest row and parsing afterwards would answer "no session has
        // been recorded in this project yet" while several resumable ones sit
        // behind the bad one.
        use event_ledger::ledger::SessionSummary;

        let good = "01a08600-0000-7000-8000-0123456789ab";
        let sessions = vec![
            SessionSummary {
                session_id: good.to_owned(),
                last_seq: 3,
                first_seen: "2026-09-09T10:00:00.000Z".to_owned(),
                last_activity: "2026-09-09T10:00:00.000Z".to_owned(),
            },
            SessionSummary {
                session_id: "not-an-id".to_owned(),
                last_seq: 9,
                first_seen: "2026-09-09T11:00:00.000Z".to_owned(),
                // Newest by activity, so a parse-last implementation stops here.
                last_activity: "2026-09-09T12:00:00.000Z".to_owned(),
            },
        ];
        assert_eq!(
            newest_usable(sessions),
            Some(good.parse().expect("fixture id")),
            "the newest *usable* session must win over a newer unusable row"
        );
        assert_eq!(newest_usable(Vec::new()), None);
    }

    #[test]
    fn an_unknown_session_id_offers_the_ids_this_project_does_have() {
        // The one thing a user in this position needs is the id they meant.
        // `rapid sessions list` cannot supply it — it reads a different
        // database (`.rapidlm/sessions.sqlite`) than interactive sessions are
        // recorded in — so resume answers the question itself.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
        assert!(
            known_sessions_hint(&ledger_path).is_none(),
            "a project with nothing recorded has nothing to offer"
        );

        let real = run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
            .expect("a session")
            .session_id
            .expect("id");
        let hint = known_sessions_hint(&ledger_path).expect("a recorded session to offer");
        assert!(
            hint.contains(&real.to_string()),
            "the hint must contain the id that is actually here: {hint}"
        );
        assert!(
            hint.contains("last activity"),
            "and enough context to pick between several: {hint}"
        );
    }

    #[test]
    fn replaying_history_consumes_every_durable_event_before_the_first_paint() {
        // The subscription is fed by a worker thread, so an empty queue means
        // "not yet", not "no more". Replay must therefore run to a known tip
        // rather than until the stream first goes quiet, or a resumed
        // transcript is silently truncated wherever the worker happened to
        // be — a race that shows a *plausible* partial history, which is
        // worse than an obvious failure.
        //
        // The history here is deliberately far longer than the subscription's
        // live bound (`event_ledger::subscription::DEFAULT_LIVE_BOUND`, 64),
        // so the worker cannot have queued it all before the first poll and
        // must refill mid-replay.
        const HISTORY: usize = 512;

        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let session_id = session.session_id;
        let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
        let actor = human_actor().expect("actor");

        let ledger = event_ledger::ledger::EventLedger::open(&ledger_path).expect("open ledger");
        let ledger_cancel = event_ledger::ledger::CancellationToken::new();
        for i in 0..HISTORY {
            ledger
                .append(
                    session_id,
                    actor.clone(),
                    // Inert in both the kernel projection and the TUI
                    // reducer, so this measures replay transport and nothing
                    // else.
                    event_ledger::event::EventKind::ModelRequested,
                    serde_json::json!({"n": i}),
                    &event_ledger::ledger::AppendOptions {
                        redaction: protocol::RedactionClass::Public,
                        trace_id: TraceId::new(),
                        expected_seq: None,
                    },
                    &ledger_cancel,
                )
                .expect("append history");
        }

        let cancel = CancellationToken::new();
        let client = InProcessKernelClient::open(&ledger_path).expect("client");
        let tip = block_on(client.get_session(session_id), &cancel)
            .expect("session")
            .seq();
        assert!(
            tip as usize > HISTORY,
            "the fixture must actually have a long history"
        );
        let mut stream = block_on(
            client.subscribe(SubscribeEvents::new(session_id, 0)),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = AppState::new();
        replay_history(&client, &mut stream, &mut ui, session_id, tip, &cancel).expect("replay");

        assert!(
            stream.cursor() >= tip,
            "replay stopped at {} of {tip} durable events — a resumed session \
             would paint a truncated transcript",
            stream.cursor()
        );
    }

    #[test]
    fn reminders_come_from_the_turn_s_own_project_root() {
        // A turn's reminder roster must be the one belonging to the project
        // the turn runs in — not whatever `.rapidlm/reminders.toml` the
        // process happens to be standing next to.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        fs::write(
            root.join(PROJECT_MARKER).join("reminders.toml"),
            "schema = \"rapidlm.reminders.v1\"\n\
             [[feed]]\n\
             name = \"house-style\"\n\
             [[feed.reminder]]\n\
             id = \"surrounding-code\"\n\
             text = \"match the surrounding code\"\n",
        )
        .expect("roster");

        let loaded = load_active_reminders(Some(&root)).expect("a readable roster");
        let (block, _floor) = loaded.expect("an always-on feed is admitted");
        assert!(
            block.contains("match the surrounding code"),
            "the project's own roster must be the one loaded: {block}"
        );

        // The root argument is what decides, not the process's location: a
        // second project with its own roster gets its own, and neither can
        // see the other's.
        let other = TempEnv::create();
        let other_root = fs::canonicalize(&other.project).expect("canonicalize");
        fs::create_dir_all(other_root.join(PROJECT_MARKER)).expect("marker");
        fs::write(
            other_root.join(PROJECT_MARKER).join("reminders.toml"),
            "schema = \"rapidlm.reminders.v1\"\n\
             [[feed]]\n\
             name = \"house-style\"\n\
             [[feed.reminder]]\n\
             id = \"other-project\"\n\
             text = \"a different project entirely\"\n",
        )
        .expect("roster");
        let (other_block, _) = load_active_reminders(Some(&other_root))
            .expect("readable")
            .expect("admitted");
        assert!(
            other_block.contains("a different project entirely")
                && !other_block.contains("match the surrounding code"),
            "each root loads its own roster and only its own: {other_block}"
        );
        assert!(
            !block.contains("a different project entirely"),
            "and the first root never saw the second's: {block}"
        );

        // A project without a roster is simply quiet, and no resolved root
        // means no project at all. (Contract, not a regression guard: with
        // no roster anywhere above the test binary's own directory, a
        // fallback would answer `None` here too.)
        let bare = root.join("elsewhere");
        fs::create_dir_all(bare.join(PROJECT_MARKER)).expect("bare");
        assert!(
            load_active_reminders(Some(&bare))
                .expect("no roster")
                .is_none()
        );
        assert!(load_active_reminders(None).expect("no root").is_none());
    }

    #[test]
    fn a_tui_only_project_s_history_is_visible_to_every_command() {
        // The defect: the TUI wrote `ledger.sqlite` and every command that
        // reads session data read `sessions.sqlite`, so `rapid sessions list`
        // showed nothing in a project that had just recorded a session.
        // Resolution is pure, so a reader sees that history *before* anything
        // is renamed.
        let env = TempEnv::create();
        let marker = env.project.join(PROJECT_MARKER);
        fs::create_dir_all(&marker).expect("marker");

        // A *real* ledger at the legacy name, with a real session in it —
        // the ledger runs in WAL mode, so a fixture of opaque bytes would
        // not exercise the sidecar handling this function exists for.
        let legacy = marker.join(LEGACY_LEDGER_NAME);
        let recorded = {
            let ledger = event_ledger::ledger::EventLedger::open(&legacy).expect("legacy ledger");
            let client = InProcessKernelClient::open(&legacy).expect("client");
            let snapshot = block_on(
                client.create_session(CreateSession::new(
                    ProjectId::new(),
                    human_actor().expect("actor"),
                    TraceId::new(),
                )),
                &CancellationToken::new(),
            )
            .expect("session");
            drop(ledger);
            snapshot.id()
        };

        assert_eq!(
            project_ledger_path(&marker),
            legacy,
            "a project whose only ledger is the legacy one must be read from it"
        );

        let notice = adopt_legacy_ledger(&marker);
        assert!(notice.is_none(), "a clean adoption is silent: {notice:?}");
        let canonical = marker.join(SESSIONS_DB_FILE);
        assert!(canonical.exists(), "the ledger moved to the canonical name");
        assert!(!legacy.exists(), "and is no longer at the old one");
        assert_eq!(project_ledger_path(&marker), canonical);
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(
                !marker
                    .join(format!("{LEGACY_LEDGER_NAME}{suffix}"))
                    .exists(),
                "no {suffix} may be orphaned at the old name"
            );
        }

        // The moved database is still a working ledger holding the same
        // session — the property a byte comparison cannot establish, and the
        // one that actually matters after a WAL-mode file is moved.
        let moved = event_ledger::ledger::EventLedger::open(&canonical).expect("moved ledger");
        let sessions = moved
            .list_sessions(&event_ledger::ledger::CancellationToken::new())
            .expect("list");
        assert!(
            sessions
                .iter()
                .any(|summary| summary.session_id == recorded.to_string()),
            "the session recorded before the move must still be there: {sessions:?}"
        );
    }

    #[test]
    fn two_ledgers_are_never_merged_or_deleted_silently() {
        // The case option C deliberately does not resolve on its own: both
        // files exist, the canonical one wins, and the user is told rather
        // than having either file quietly chosen or destroyed.
        let env = TempEnv::create();
        let marker = env.project.join(PROJECT_MARKER);
        fs::create_dir_all(&marker).expect("marker");
        let legacy = marker.join(LEGACY_LEDGER_NAME);
        let canonical = marker.join(SESSIONS_DB_FILE);
        fs::write(&legacy, b"older transcripts").expect("legacy");
        fs::write(&canonical, b"canonical rows").expect("canonical");

        let notice = adopt_legacy_ledger(&marker);
        // Data first: `fs::rename` overwrites its destination, so an
        // unguarded adoption here destroys the canonical ledger outright.
        assert_eq!(
            fs::read(&canonical).expect("the canonical ledger must still be there"),
            b"canonical rows",
            "adoption must never overwrite an existing canonical ledger"
        );
        assert_eq!(
            fs::read(&legacy).expect("the legacy ledger must still be there"),
            b"older transcripts",
            "and must never consume the legacy one"
        );
        assert_eq!(project_ledger_path(&marker), canonical);
        let notice = notice.expect("the user must be told which one is in use");
        assert!(
            notice.contains(&legacy.display().to_string())
                && notice.contains("Nothing has been deleted"),
            "the notice must name the untouched file and say it survives: {notice}"
        );
    }

    #[test]
    fn a_stray_sidecar_does_not_cost_the_transactions_it_may_hold() {
        // The ledger runs in WAL mode, so a `-wal` beside the database can
        // hold committed transactions the main file does not have yet:
        // renaming the main file alone would roll the ledger back to its last
        // checkpoint, silently. Adoption therefore checkpoints first, by
        // opening the ledger and letting the handle drop (`EventLedger` keeps
        // no persistent connection), and only then moves one complete file.
        let env = TempEnv::create();
        let marker = env.project.join(PROJECT_MARKER);
        fs::create_dir_all(&marker).expect("marker");
        let legacy = marker.join(LEGACY_LEDGER_NAME);
        let recorded = {
            let client = InProcessKernelClient::open(&legacy).expect("client");
            block_on(
                client.create_session(CreateSession::new(
                    ProjectId::new(),
                    human_actor().expect("actor"),
                    TraceId::new(),
                )),
                &CancellationToken::new(),
            )
            .expect("session")
            .id()
        };
        fs::write(
            marker.join(format!("{LEGACY_LEDGER_NAME}-wal")),
            b"a sidecar left lying around",
        )
        .expect("sidecar");

        adopt_legacy_ledger(&marker);
        let canonical = marker.join(SESSIONS_DB_FILE);
        assert_eq!(
            project_ledger_path(&marker),
            canonical,
            "the project must end up on exactly one ledger"
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(
                !marker
                    .join(format!("{LEGACY_LEDGER_NAME}{suffix}"))
                    .exists(),
                "no {suffix} may be orphaned at the old name, pointing at a database \
that is no longer there"
            );
        }
        let sessions = event_ledger::ledger::EventLedger::open(&canonical)
            .expect("the adopted ledger still opens")
            .list_sessions(&event_ledger::ledger::CancellationToken::new())
            .expect("list");
        assert!(
            sessions
                .iter()
                .any(|summary| summary.session_id == recorded.to_string()),
            "and every session recorded before the move is still in it: {sessions:?}"
        );
    }

    #[test]
    fn a_legacy_file_that_is_not_a_ledger_is_reported_rather_than_moved() {
        // Adoption never moves something it could not open: a file at the
        // legacy name that is not a database stays exactly where it is, named
        // in a notice, rather than being renamed onto the path every command
        // will then try to use as the project's ledger.
        let env = TempEnv::create();
        let marker = env.project.join(PROJECT_MARKER);
        fs::create_dir_all(&marker).expect("marker");
        let legacy = marker.join(LEGACY_LEDGER_NAME);
        fs::write(&legacy, b"this is not a sqlite database at all").expect("legacy");

        let notice = adopt_legacy_ledger(&marker);
        assert!(legacy.exists(), "the file stays where it is");
        assert!(
            !marker.join(SESSIONS_DB_FILE).exists(),
            "and is not renamed onto the canonical path"
        );
        let notice = notice.expect("the user must be told");
        assert!(
            notice.contains("could not be opened"),
            "the notice must say why: {notice}"
        );
    }

    #[test]
    fn a_fresh_project_uses_the_canonical_name() {
        let env = TempEnv::create();
        let marker = env.project.join(PROJECT_MARKER);
        fs::create_dir_all(&marker).expect("marker");
        assert!(adopt_legacy_ledger(&marker).is_none());
        assert_eq!(project_ledger_path(&marker), marker.join(SESSIONS_DB_FILE));
    }

    #[test]
    fn every_command_resolves_the_same_project_from_any_subdirectory() {
        // `rapid goal show` run in `src/` must read the project's goal, not
        // report "no active goal" and leave a stray `.rapidlm/` behind. The
        // TUI already walked up to the nearest marker; this is the same rule
        // for everything else.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        let nested = root.join("crates").join("deep").join("src");
        fs::create_dir_all(&nested).expect("nested dirs");

        let from_root = project_path_in(&root, GOAL_FILE);
        let from_nested = project_path_in(&nested, GOAL_FILE);
        assert_eq!(
            from_root, from_nested,
            "a subdirectory must resolve the same project file as the root"
        );
        assert_eq!(from_root, root.join(PROJECT_MARKER).join(GOAL_FILE));
        assert!(
            !from_nested.starts_with(&nested),
            "no command may create a second project store inside a subdirectory: {}",
            from_nested.display()
        );
    }

    #[test]
    fn a_git_checkout_without_a_rapidlm_directory_still_resolves_to_its_root() {
        // A first run in a fresh clone has no `.rapidlm` yet. The git marker
        // is what makes `rapid goal create` from a subdirectory put the
        // project store at the repository root rather than beside whatever
        // file the developer happened to be editing.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::remove_dir_all(root.join(PROJECT_MARKER)).ok();
        fs::create_dir_all(root.join(GIT_MARKER)).expect("git marker");
        let nested = root.join("src");
        fs::create_dir_all(&nested).expect("nested");
        assert_eq!(
            project_path_in(&nested, GOAL_FILE),
            root.join(PROJECT_MARKER).join(GOAL_FILE)
        );
    }

    #[test]
    fn a_bare_directory_still_resolves_beneath_itself() {
        // No marker anywhere: the fallback must stay exactly what every one
        // of these call sites did before — a `.rapidlm` in the working
        // directory — so `rapid goal create` in a scratch directory keeps
        // working.
        let dir = TempEnv::create();
        let bare = fs::canonicalize(&dir.project)
            .expect("canonicalize")
            .join("scratch");
        fs::create_dir_all(&bare).expect("scratch");
        fs::remove_dir_all(dir.project.join(PROJECT_MARKER)).ok();
        let resolved = project_path_in(&bare, GOAL_FILE);
        assert!(
            resolved.starts_with(&bare)
                || resolved.starts_with(fs::canonicalize(&dir.project).expect("c")),
            "an unmarked directory falls back to itself or its nearest marker: {}",
            resolved.display()
        );
        assert!(resolved.ends_with(Path::new(PROJECT_MARKER).join(GOAL_FILE)));
    }

    #[test]
    fn a_project_can_be_opened_again_after_a_session_ends() {
        // The most ordinary thing a user does: run `rapid`, quit, run it
        // again. The second run must get its own session rather than
        // colliding with the first one's records.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let first =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("first run")
                .session_id
                .expect("id");
        let second =
            run_interactive(env.options(vec![InteractiveInput::Submit("/quit".to_owned())]))
                .expect("running rapid a second time in the same project must work")
                .session_id
                .expect("id");
        assert_ne!(first, second, "each run gets its own session");
    }

    #[test]
    fn the_memory_panel_shows_exactly_what_the_model_is_given() {
        // The panel's only claim is "this is what the model knows", so it
        // must render the same bounded text `load_memory_index` hands the
        // turn — not a second read of the file with its own bounds, which
        // would answer that question with something the model never saw.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        // Deliberately past `MAX_MEMORY_INDEX_LINES`, so a raw read of the
        // file and the bounded read the model gets actually differ — with a
        // short fixture this test passes even when the panel reads the file
        // itself, which is exactly the mistake it exists to catch.
        let mut index = String::from("- [Auth notes](auth.md) — where the tokens live\n");
        for i in 0..crate::host::MAX_MEMORY_INDEX_LINES + 50 {
            index.push_str(&format!("- [Note {i}](note{i}.md) — filler\n"));
        }
        fs::write(root.join(PROJECT_MARKER).join("MEMORY.md"), &index).expect("memory index");

        let mut ui = AppState::new();
        sync_memory_index(&mut ui, &root);
        let given_to_model: Vec<String> = crate::host::load_memory_index(&root)
            .expect("the index loads")
            .lines()
            .map(str::to_owned)
            .collect();
        // Lengths first: a mismatch here is the whole failure, and comparing
        // the vectors straight away buries it under two hundred identical
        // lines of fixture.
        assert_eq!(
            ui.memory().len(),
            given_to_model.len(),
            "the panel and the model must be shown the same number of lines"
        );
        assert_eq!(
            ui.memory(),
            given_to_model,
            "the panel and the model must be shown the same text"
        );

        assert!(
            ui.memory().len() <= crate::host::MAX_MEMORY_INDEX_LINES,
            "the panel must not show more than the model was given: {}",
            ui.memory().len()
        );
        assert!(
            index.lines().count() > ui.memory().len(),
            "the fixture must actually exceed the bound, or this test proves nothing"
        );

        // A project without one projects nothing rather than an empty row.
        let bare = TempEnv::create();
        let mut ui = AppState::new();
        sync_memory_index(&mut ui, &bare.project);
        assert!(ui.memory().is_empty());
    }

    #[test]
    fn the_models_panel_marks_the_running_model_and_never_carries_a_credential() {
        // `/models` opened an empty panel while the configuration it should
        // describe was sitting in `[model.<id>]` tables the whole time.
        use crate::user_config::{ConfigProvider, ModelEntry, ModelsSection, UserConfig};

        let entry = |model: &str, window: Option<u32>| ModelEntry {
            provider: ConfigProvider::Anthropic,
            model: model.to_owned(),
            base_url: "https://example.invalid/v1".to_owned(),
            name: None,
            // Configured, and it must not survive into the projection.
            api_key: Some("sk-do-not-render-me".to_owned()),
            env_key: vec!["SOME_KEY".to_owned()],
            max_tokens: None,
            context_window: window,
            reasoning_effort: None,
        };
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("big".to_owned(), entry("claude-opus-5", Some(200_000)));
        entries.insert("backup".to_owned(), entry("gpt-5", None));
        let config = UserConfig {
            models: ModelsSection {
                default: Some("big".to_owned()),
                entries,
                fallback: vec!["backup".to_owned()],
            },
            phases: Default::default(),
            unknown_keys: Vec::new(),
        };

        let rows = model_rows(&config, Some("big"));
        let big = rows.iter().find(|row| row.id == "big").expect("big");
        assert!(
            big.active,
            "the resolved model must be marked as the one that runs"
        );
        assert_eq!(big.context_window, Some(200_000));
        assert_eq!(big.fallback_rank, None);

        let backup = rows.iter().find(|row| row.id == "backup").expect("backup");
        assert!(!backup.active);
        assert_eq!(
            backup.fallback_rank,
            Some(0),
            "a fallback must carry its position in the chain, which is the \
question the panel answers"
        );

        // The projection has nowhere to put a credential, and this asserts
        // the whole rendered surface rather than trusting that.
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Models,
            &reduce(
                AppState::new(),
                &UiEvent::Local(LocalUiEvent::SyncModels(rows)),
            ),
            120,
            8,
            &tui::state::CancellationToken::new(),
        )
        .join("\n");
        assert!(
            !painted.contains("sk-do-not-render-me") && !painted.contains("SOME_KEY"),
            "no credential may reach a rendered frame: {painted}"
        );
    }

    /// A `SessionLoop` over `session` for slash-command tests, with the
    /// stream/ui/renderer locals the loop borrows created here. The loop
    /// borrows `session` immutably, so a test that also needs
    /// `session.run_turn` runs those turns before or after the loop.
    struct LoopLocals {
        stream: EventStream,
        ui: AppState,
        cancel: CancellationToken,
        interrupt_count: u32,
        saw_ctrl_c: bool,
        renderer: TuiRenderer,
    }

    impl LoopLocals {
        fn for_session(session: &ScriptedSession) -> Self {
            let cancel = CancellationToken::new();
            let snapshot =
                block_on(session.client.get_session(session.session_id), &cancel).expect("session");
            let stream = block_on(
                session
                    .client
                    .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
                &cancel,
            )
            .expect("subscribe");
            Self {
                stream,
                ui: reduce(AppState::new(), &UiEvent::Snapshot(snapshot)),
                cancel,
                interrupt_count: 0,
                saw_ctrl_c: false,
                renderer: TuiRenderer::new(true),
            }
        }

        fn session_loop<'a>(
            &'a mut self,
            session: &'a ScriptedSession,
            backings: Vec<ScriptedModel>,
        ) -> SessionLoop<'a> {
            autonomous_session_loop(
                session,
                &mut self.stream,
                &mut self.ui,
                &self.cancel,
                &mut self.interrupt_count,
                &mut self.saw_ctrl_c,
                &mut self.renderer,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                scripted_backing_queue(backings),
            )
        }
    }

    /// Drive the loop's own per-tick work until the compaction it started
    /// has settled and its record has reached the transcript (or not, when
    /// `expect_record` is false).
    fn drain_until_compaction_settles(loop_state: &mut SessionLoop<'_>, expect_record: bool) {
        for _ in 0..600 {
            loop_state.drain().expect("drain");
            let settled = loop_state.compaction.is_none();
            let recorded = loop_state
                .ui
                .transcript()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Compacted { .. }));
            if settled && (recorded || !expect_record) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "compaction did not settle: {:?}",
            loop_state.ui.transcript()
        );
    }

    fn command_outputs(ui: &AppState) -> Vec<&str> {
        ui.transcript()
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::CommandOutput { text }
                | TranscriptEntry::CommandError { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn an_interactive_turn_runs_the_projects_hooks_reminders_and_retrieval() {
        // The same `.rapidlm/settings.json`, `reminders.toml` and repo that
        // `rapid exec` honours were silently ignored by an interactive
        // turn: no hooks, no reminders, no proactive retrieval. One setup
        // now serves both paths.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        // A pre-tool hook that denies every call, naming itself.
        fs::write(
            root.join(PROJECT_MARKER).join("settings.json"),
            r#"{"hooks":{"pre_tool_use":["echo 'house rule: no writes today' >&2; exit 1"]}}"#,
        )
        .expect("settings");
        fs::write(
            root.join(PROJECT_MARKER).join("reminders.toml"),
            "schema = \"rapidlm.reminders.v1\"\n\
             [[feed]]\n\
             name = \"house-style\"\n\
             [[feed.reminder]]\n\
             id = \"surrounding-code\"\n\
             text = \"match the surrounding code\"\n",
        )
        .expect("roster");
        fs::write(
            root.join("lru.py"),
            "class LRUCache:\n    def get(self, key):\n        return self._data.get(key)\n",
        )
        .expect("seed");

        let mut session = ScriptedSession::create(&env);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "how does LRUCache eviction work? write notes.txt about it",
            ScriptedModel::write_then_answer("notes.txt", "eviction notes", "done")
                .capturing_blocks(seen.clone()),
        );

        // The hook denied the write: no file, and the denial in the
        // transcript with the hook's own reason.
        assert!(
            !root.join("notes.txt").exists(),
            "the pre_tool_use hook must deny the write in an interactive turn"
        );
        assert!(
            session.transcript().iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity {
                    tool,
                    status: ToolActivityStatus::Denied,
                    detail: Some(detail),
                } if tool == crate::exec_tools::WORKSPACE_WRITE_TOOL
                    && detail.contains("house rule: no writes today")
            )),
            "{:?}",
            session.transcript()
        );

        // Reminders and retrieval reached the packet.
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        let locators: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert!(
            seen.iter()
                .any(|(l, t)| l == "reminders/active" && t.contains("match the surrounding code")),
            "{locators:?}"
        );
        assert!(
            seen.iter()
                .any(|(l, t)| l.starts_with("retrieved:") && t.contains("LRUCache")),
            "{locators:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn slash_compact_fires_the_projects_pre_and_post_compact_hooks() {
        // The hook registry has carried `CompactPre`/`CompactPost` since
        // P9-012 with nothing firing them. Both observe a `/compact` now,
        // with the turn count and, after, the summary's size — on a trusted
        // project only, like every hook.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        // Hooks run in the process's working directory, as headless hooks
        // do: absolute targets.
        let pre_path = root.join("pre.json");
        let post_path = root.join("post.json");
        fs::write(
            root.join(PROJECT_MARKER).join("settings.json"),
            format!(
                r#"{{"hooks":{{"pre_compact":["cat > {}"],"post_compact":["cat > {}"]}}}}"#,
                pre_path.display(),
                post_path.display()
            ),
        )
        .expect("settings");
        let mut session = ScriptedSession::create(&env);
        session.run_turn("one", ScriptedModel::terminal("uno"));
        session.run_turn("two", ScriptedModel::terminal("dos"));
        let mut locals = LoopLocals::for_session(&session);
        let mut loop_state =
            locals.session_loop(&session, vec![ScriptedModel::terminal("the two turns")]);
        loop_state.dispatch_slash("/compact").expect("compact");
        drain_until_compaction_settles(&mut loop_state, true);

        let pre: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join("pre.json")).expect("pre hook ran"))
                .expect("json on stdin");
        assert_eq!(pre["event"], "pre_compact");
        assert_eq!(pre["turns"], 2);
        assert_eq!(pre["trigger"], "manual");
        let post: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join("post.json")).expect("post hook ran"),
        )
        .expect("json on stdin");
        assert_eq!(post["event"], "post_compact");
        assert_eq!(post["turns"], 2);
        assert_eq!(post["summary_bytes"], "the two turns".len());
    }

    /// A subagent runner that runs until its token is cancelled, then
    /// reports a cancelled run — the shape of a child that would otherwise
    /// take a while.
    struct RunsUntilCancelled {
        started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::exec_tools::SubagentRunner for RunsUntilCancelled {
        fn run(
            &self,
            _prompt: &str,
            _agent_type: &str,
            _write_scope: Option<&str>,
            cancel: &agent_runtime::CancellationToken,
        ) -> Result<crate::exec_tools::SubagentReport, String> {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            for _ in 0..2_000 {
                if cancel.is_cancelled() {
                    return Ok(crate::exec_tools::SubagentReport {
                        summary: "stopped before finishing".to_owned(),
                        status: "cancelled".to_owned(),
                        tool_calls: 0,
                        tokens: 0,
                        cost_usd_micros: None,
                        stop_reason: Some("cancelled".to_owned()),
                        claims: Vec::new(),
                        blockers: Vec::new(),
                        open_questions: Vec::new(),
                        patch_summary: None,
                        artifacts: Vec::new(),
                    });
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err("the child was never cancelled".to_owned())
        }
    }

    #[test]
    fn slash_agents_cancel_stops_a_running_subagent_that_the_panel_shows() {
        // `/agents cancel` was refused — no running-agent registry — and
        // `/agents` showed nothing for the one kind of agent the binary
        // runs, because a `task_spawn` child left no `agent.*` event. Now
        // the child is recorded, the panel shows it running, and the loop
        // stops it by id while its parent turn goes on.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut locals = LoopLocals::for_session(&session);
        let mut loop_state = locals.session_loop(
            &session,
            vec![ScriptedModel::call_then_answer(
                crate::exec_tools::TASK_SPAWN_TOOL,
                serde_json::json!({"prompt": "survey the tests", "type": "explore"}),
                "the child reported",
            )],
        );
        loop_state.shared.scripted_subagents = Some(std::sync::Arc::new(RunsUntilCancelled {
            started: started.clone(),
        }));
        loop_state.submit_turn("delegate").expect("submit");

        // The child is running and the panel knows: one agent, Running,
        // with its role and task.
        let mut running_id = None;
        for _ in 0..600 {
            loop_state.drain().expect("drain");
            if let Some(agent) = loop_state
                .ui
                .agents()
                .values()
                .find(|agent| agent.state() == tui::state::AgentLifecycle::Running)
            {
                running_id = Some(agent.id());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let running_id = running_id.unwrap_or_else(|| {
            panic!(
                "the child should show as running: {:?} / error {:?} / transcript {:?}",
                loop_state.ui.agents(),
                loop_state.ui.protocol_error(),
                loop_state.ui.transcript()
            )
        });
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
        let row = &loop_state.ui.agents()[&running_id];
        assert_eq!(row.role(), Some("explore"));
        assert_eq!(row.current_operation(), Some("survey the tests"));
        assert_eq!(loop_state.shared.agents.running(), vec![running_id]);

        // Cancel it by id — the child ends, the parent turn goes on.
        loop_state
            .dispatch_slash(&format!("/agents cancel {running_id}"))
            .expect("cancel");
        assert!(
            command_outputs(loop_state.ui)
                .iter()
                .any(|t| *t == format!("cancelled agent {running_id}")),
            "{:?}",
            command_outputs(loop_state.ui)
        );
        for _ in 0..600 {
            loop_state.drain().expect("drain");
            if !loop_state.model_busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        settle_last_turn(&mut loop_state);
        assert_eq!(
            loop_state.ui.agents()[&running_id].state(),
            tui::state::AgentLifecycle::Cancelled,
            "{:?}",
            loop_state.ui.agents()
        );
        assert!(loop_state.shared.agents.running().is_empty());
        assert!(
            loop_state.ui.transcript().iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Assistant { text } if text == "the child reported"
            )),
            "the parent turn finished on its own: {:?}",
            loop_state.ui.transcript()
        );
        // Cancelling again names nothing.
        loop_state
            .dispatch_slash(&format!("/agents cancel {running_id}"))
            .expect("cancel again");
        assert!(
            command_outputs(loop_state.ui)
                .iter()
                .any(|t| *t == format!("no running agent {running_id}")),
            "{:?}",
            command_outputs(loop_state.ui)
        );
        loop_state.dispatch_slash("/agents cancel").expect("bare");
        assert!(
            command_outputs(loop_state.ui).contains(&"no running agents to cancel"),
            "{:?}",
            command_outputs(loop_state.ui)
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_interactive_sessions_mcp_servers_are_started_once_and_serve_every_turn() {
        // Each turn used to spawn, handshake and kill every configured MCP
        // server — the start-up cost on every turn, and any state the
        // server held gone between them. The session owns the connections
        // now; a turn reuses them.
        const SERVER_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, json, os
with open(os.environ["MCP_STARTS_LOG"], "a") as log:
    log.write("started\n")
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
calls = 0
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo", "version": "1.0"}}})
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": [
            {"name": "count", "description": "how many calls this process has served",
             "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        calls += 1
        send({"jsonrpc": "2.0", "id": rid, "result": {"content": [
            {"type": "text", "text": "call number " + str(calls)}]}})
"#;
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        let script_path = root.join("mcp-count-server.py");
        fs::write(&script_path, SERVER_SCRIPT).expect("write server");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let starts_log = root.join("mcp-starts.log");
        fs::write(
            root.join(PROJECT_MARKER).join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "demo": {
                        "command": "python3",
                        "args": [script_path.display().to_string()],
                        "env": {"MCP_STARTS_LOG": starts_log.display().to_string()}
                    }
                }
            })
            .to_string(),
        )
        .expect("settings");

        let mut session = ScriptedSession::create(&env);
        for turn in 1..=2 {
            session.run_turn(
                &format!("turn {turn}"),
                ScriptedModel::call_then_answer(
                    "mcp__demo__count",
                    serde_json::json!({}),
                    &format!("answered {turn}"),
                ),
            );
        }
        let completed = session
            .transcript()
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolActivity {
                        tool,
                        status: ToolActivityStatus::Completed,
                        ..
                    } if tool == "mcp__demo__count"
                )
            })
            .count();
        assert_eq!(
            completed,
            2,
            "the MCP tool served both turns: {:?}",
            session.transcript()
        );
        let starts = fs::read_to_string(&starts_log).unwrap_or_default();
        assert_eq!(
            starts.lines().count(),
            1,
            "one server process for the session, not one per turn: {starts:?}"
        );
    }

    #[test]
    fn slash_compact_folds_the_earlier_turns_into_a_summary_the_next_turn_carries() {
        // `/compact` said "not wired yet" while a long session silently
        // lost its oldest turns to the memory partition. Now it asks the
        // model for a summary of the turns so far, records it, and every
        // turn after reads the summary where the turns were.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "call the project Nightjar from now on",
            ScriptedModel::terminal("Noted: the project is Nightjar."),
        );
        session.run_turn(
            "and make the tests pass",
            ScriptedModel::terminal("Two tests fixed, one still red."),
        );

        let summarizer_saw = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut locals = LoopLocals::for_session(&session);
        {
            let mut loop_state = locals.session_loop(
                &session,
                vec![
                    ScriptedModel::terminal("Project is Nightjar; one test still red.")
                        .capturing_blocks(summarizer_saw.clone()),
                ],
            );
            loop_state.dispatch_slash("/compact").expect("compact");
            // Not asserted here: whether the thread is still running when
            // dispatch returns — a scripted model can finish inside the
            // dispatch's own trailing drain. `ctrl_c_cancels_a_running_
            // compaction` holds the thread open to look at that state.
            drain_until_compaction_settles(&mut loop_state, true);

            let outputs = command_outputs(loop_state.ui);
            assert!(
                outputs.iter().any(|t| t.starts_with("compacting")),
                "{outputs:?}"
            );
            assert!(
                outputs
                    .iter()
                    .any(|t| t.starts_with("compacted 2 turn(s) in 1 tokens")),
                "{outputs:?}"
            );
            assert!(
                loop_state.ui.transcript().iter().any(|entry| matches!(
                    entry,
                    TranscriptEntry::Compacted { turns: 2, summary }
                        if summary == "Project is Nightjar; one test still red."
                )),
                "the record reaches the transcript through the ledger: {:?}",
                loop_state.ui.transcript()
            );
        }

        // What the summarizer was asked: both turns, and the instructions.
        let summarizer_saw = summarizer_saw.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            summarizer_saw
                .iter()
                .any(|(l, t)| l == "system/compaction" && t.contains("summary")),
            "{:?}",
            summarizer_saw
                .iter()
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            summarizer_saw
                .iter()
                .any(|(_, t)| t.contains("call the project Nightjar from now on"))
                && summarizer_saw
                    .iter()
                    .any(|(_, t)| t.contains("Two tests fixed, one still red.")),
            "{summarizer_saw:?}"
        );

        // The ledger now reads as the summary and no turns.
        let history = conversation_history(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(
            history.summary.as_deref(),
            Some("Project is Nightjar; one test still red.")
        );
        assert!(history.turns.is_empty(), "{:?}", history.turns);

        // The next turn carries the summary where the turns were, and is
        // told so.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "what is still red?",
            ScriptedModel::terminal("One test.").capturing_blocks(seen.clone()),
        );
        {
            let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
            let locators: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
            assert!(
                seen.iter()
                    .any(|(l, t)| l == crate::host::COMPACTION_LOCATOR
                        && t == "Project is Nightjar; one test still red."),
                "{locators:?}"
            );
            assert!(
                seen.iter()
                    .any(|(l, _)| l == crate::host::POST_COMPACTION_LOCATOR),
                "{locators:?}"
            );
            assert!(
                !locators
                    .iter()
                    .any(|l| l.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX)),
                "the folded turns are not carried as turns too: {locators:?}"
            );
            assert!(
                !seen
                    .iter()
                    .any(|(_, t)| t.contains("Noted: the project is Nightjar.")),
                "and their text is gone from the packet"
            );
        }

        // And the turn after that carries the summary, then the one turn
        // since it.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "fix it",
            ScriptedModel::terminal("Fixed.").capturing_blocks(seen.clone()),
        );
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        let turns: Vec<&str> = seen
            .iter()
            .filter(|(l, _)| l.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert!(turns[0].contains("what is still red?") && turns[0].contains("One test."));
        let summary_at = seen
            .iter()
            .position(|(l, _)| l == crate::host::COMPACTION_LOCATOR)
            .expect("summary");
        let turn_at = seen
            .iter()
            .position(|(l, _)| l.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .expect("turn");
        assert!(summary_at < turn_at, "the summary reads first");
    }

    #[test]
    fn slash_compact_with_nothing_to_fold_says_so_and_calls_no_model() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let mut locals = LoopLocals::for_session(&session);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut loop_state = locals.session_loop(
            &session,
            vec![ScriptedModel::terminal("unused").capturing_blocks(seen.clone())],
        );
        loop_state.dispatch_slash("/compact").expect("compact");
        drain_until_compaction_settles(&mut loop_state, false);
        assert_eq!(
            command_outputs(loop_state.ui),
            vec![
                "compacting... (Ctrl-C cancels)",
                "nothing to compact: the session has no completed turns yet"
            ]
        );
        assert!(
            seen.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "no model call was made"
        );
    }

    #[test]
    fn a_turn_that_overflowed_records_the_summary_its_recovery_wrote() {
        // The in-turn overflow recovery folds the earlier turns into a
        // summary; if that summary were kept only in the turn's own packet,
        // the next turn would read the same turns, overflow the same way
        // and pay for the same recovery — every turn, silently. So the
        // turn records it, and the next turn carries it.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        // Long enough answers that a summary is genuinely smaller than the
        // turns it stands for — a recovery whose summary would not shrink
        // the packet sets it aside, by design.
        let long_answer = |lead: &str| format!("{lead} {}", "and so on, ".repeat(120));
        session.run_turn(
            "name it Nightjar",
            ScriptedModel::terminal(&long_answer("Named.")),
        );
        session.run_turn(
            "add tests",
            ScriptedModel::terminal(&long_answer("Added two.")),
        );

        session.run_turn(
            "run them",
            ScriptedModel::overflow_then_summary_then_answer(
                "Nightjar; two tests added.",
                "Both pass.",
            ),
        );
        assert!(
            session.transcript().iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Compacted { turns: 2, summary }
                    if summary == "Nightjar; two tests added."
            )),
            "the recovery's summary is recorded and shown: {:?}",
            session.transcript()
        );
        let history = conversation_history(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(
            history.summary.as_deref(),
            Some("Nightjar; two tests added.")
        );
        let carried: Vec<&str> = history.turns.iter().map(|t| t.user()).collect();
        assert_eq!(
            carried,
            vec!["run them"],
            "the overflowing turn itself is after the summary, so it is still a turn"
        );

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn(
            "and lint",
            ScriptedModel::terminal("Clean.").capturing_blocks(seen.clone()),
        );
        let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            seen.iter()
                .any(|(l, t)| l == crate::host::COMPACTION_LOCATOR
                    && t == "Nightjar; two tests added."),
            "{:?}",
            seen.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>()
        );
        let turns: Vec<&str> = seen
            .iter()
            .filter(|(l, _)| l.starts_with(crate::host::CONVERSATION_LOCATOR_PREFIX))
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert!(turns[0].contains("run them"), "{turns:?}");
    }

    #[test]
    fn a_compaction_older_than_the_read_window_still_reaches_the_turn() {
        // The reader walks a bounded window of the newest events. A
        // compaction older than that used to fall out of it — and the
        // summary was the session's whole memory of everything before.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("one", ScriptedModel::terminal("uno"));
        let cancel = CancellationToken::new();
        let tip = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        session
            .client
            .append_turn_progress(
                session.session_id,
                &session.actor,
                TraceId::new(),
                event_ledger::event::EventKind::ContextCompacted,
                serde_json::json!({"summary": "the first turn", "through_seq": tip, "turns": 1}),
            )
            .expect("record");
        session.run_turn("two", ScriptedModel::terminal("dos"));
        session.run_turn("three", ScriptedModel::terminal("tres"));
        // A window that reaches back only to the newest turn's events.
        let history = conversation_history_within(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
            6,
        );
        assert_eq!(history.summary.as_deref(), Some("the first turn"));
        let carried: Vec<&str> = history.turns.iter().map(|t| t.user()).collect();
        assert_eq!(carried, vec!["three"], "only what the window reaches");
        // The full window reads the same summary and both later turns.
        let history = conversation_history(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(history.summary.as_deref(), Some("the first turn"));
        let carried: Vec<&str> = history.turns.iter().map(|t| t.user()).collect();
        assert_eq!(carried, vec!["two", "three"]);
    }

    #[test]
    fn a_compaction_the_model_fails_is_reported_and_changes_nothing() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("one", ScriptedModel::terminal("uno"));
        let mut locals = LoopLocals::for_session(&session);
        {
            // A `ScriptedModel` with no outputs answers every step with a
            // typed failure.
            let mut loop_state = locals.session_loop(&session, vec![ScriptedModel::default()]);
            loop_state.dispatch_slash("/compact").expect("compact");
            drain_until_compaction_settles(&mut loop_state, false);
            let outputs = command_outputs(loop_state.ui);
            assert!(
                outputs
                    .iter()
                    .any(|t| t.starts_with("compaction failed: the model call failed")),
                "{outputs:?}"
            );
            assert!(
                !loop_state
                    .ui
                    .transcript()
                    .iter()
                    .any(|entry| matches!(entry, TranscriptEntry::Compacted { .. })),
                "nothing was recorded"
            );
            // The slot is free again: a turn can be submitted.
            assert!(!loop_state.model_busy());
        }
        let history = conversation_history(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(history.summary, None);
        assert_eq!(history.turns.len(), 1);
    }

    #[test]
    fn ctrl_c_cancels_a_running_compaction() {
        // A compaction is not a turn, so the kernel interrupt Ctrl-C sends
        // cannot reach it; its own token must.
        struct UntilCancelled;
        impl crate::host::LiveModelCall for UntilCancelled {
            fn step(
                &mut self,
                _blocks: &[context_engine::compile::ContextBlock],
                _input: &ModelStepInput<'_>,
                cancel: &agent_runtime::CancellationToken,
            ) -> Result<ModelStepOutput, ModelStepError> {
                for _ in 0..2_000 {
                    if cancel.is_cancelled() {
                        return Err(ModelStepError::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(ModelStepOutput::Terminal {
                    text: "never cancelled".to_owned(),
                    tokens: 1,
                    cost_usd_micros: None,
                })
            }
        }
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("one", ScriptedModel::terminal("uno"));
        let mut locals = LoopLocals::for_session(&session);
        let backings: ScriptedBackingQueue =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
                vec![Box::new(UntilCancelled) as Box<dyn crate::host::LiveModelCall + Send>],
            )));
        let mut loop_state = autonomous_session_loop(
            &session,
            &mut locals.stream,
            &mut locals.ui,
            &locals.cancel,
            &mut locals.interrupt_count,
            &mut locals.saw_ctrl_c,
            &mut locals.renderer,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            backings,
        );
        loop_state.dispatch_slash("/compact").expect("compact");
        assert!(loop_state.compaction.is_some());
        // Plain text while it runs is held back, as it is during a turn.
        assert!(loop_state.model_busy());
        assert!(matches!(
            loop_state
                .handle_input(InteractiveInput::CtrlC)
                .expect("ctrl-c"),
            LoopControl::Continue
        ));
        drain_until_compaction_settles(&mut loop_state, false);
        let outputs = command_outputs(loop_state.ui);
        assert!(
            outputs.contains(&"compaction failed: cancelled"),
            "{outputs:?}"
        );
        assert!(!loop_state.model_busy());
    }

    #[test]
    fn a_compaction_covers_the_turns_through_its_seq_and_is_followed_through_a_fork() {
        // The fold rule on the reader, against the real ledger: a
        // `context.compacted` covers the turns recorded through its
        // `through_seq` — a turn another process landed after that seq is
        // still a turn — and a child forked after it reads the summary
        // from its parent.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("one", ScriptedModel::terminal("uno"));
        let cancel = CancellationToken::new();
        let after_first =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        session.run_turn("two", ScriptedModel::terminal("dos"));
        session
            .client
            .append_turn_progress(
                session.session_id,
                &session.actor,
                TraceId::new(),
                event_ledger::event::EventKind::ContextCompacted,
                serde_json::json!({
                    "summary": "only the first turn",
                    "through_seq": after_first.seq(),
                    "turns": 1,
                }),
            )
            .expect("record a compaction that covers only the first turn");
        let history = conversation_history(
            &session.client,
            session.session_id,
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(history.summary.as_deref(), Some("only the first turn"));
        let carried: Vec<&str> = history.turns.iter().map(|t| t.user()).collect();
        assert_eq!(carried, vec!["two"]);

        // Fork after the compaction: the child starts from the summary.
        let tip = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        assert_eq!(
            history.through_seq, tip,
            "the history names the tip it was read through"
        );
        let child = block_on(
            session.client.fork_session(ForkSession::new(
                session.session_id,
                tip,
                session.actor.clone(),
                TraceId::new(),
            )),
            &cancel,
        )
        .expect("fork");
        let child_history = conversation_history(
            &session.client,
            child.id(),
            &agent_runtime::CancellationToken::new(),
        );
        assert_eq!(
            child_history.summary.as_deref(),
            Some("only the first turn")
        );
        let carried: Vec<&str> = child_history.turns.iter().map(|t| t.user()).collect();
        assert_eq!(carried, vec!["two"]);
    }

    #[test]
    fn a_rewound_session_shows_the_transcript_up_to_the_rewind_point() {
        // `/rewind` forks at the seq and switches to the child, whose own
        // ledger starts at `session.forked` — so the transcript came up
        // empty, a session with no past rather than one rewound to a point
        // in it. The model already remembered the turns up to the rewind
        // (`a_rewound_session_remembers_…`); now the screen agrees.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("first", ScriptedModel::terminal("first answer"));
        let cancel = CancellationToken::new();
        let after_first = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        session.run_turn("second", ScriptedModel::terminal("second answer"));

        let mut locals = LoopLocals::for_session(&session);
        let mut loop_state = locals.session_loop(&session, Vec::new());
        loop_state
            .dispatch_slash(&format!("/rewind {after_first}"))
            .expect("rewind");
        let child_id = loop_state.session_id;
        assert_ne!(child_id, session.session_id);
        let shown: Vec<String> = loop_state
            .ui
            .transcript()
            .iter()
            .map(|entry| format!("{entry:?}"))
            .collect();
        let position = |needle: &str| shown.iter().position(|line| line.contains(needle));
        assert!(
            position("\"first\"").is_some() && position("first answer").is_some(),
            "the turn before the rewind point is shown: {shown:?}"
        );
        assert!(
            position("second").is_none(),
            "and nothing after it: {shown:?}"
        );
        assert!(
            position("rewound to seq").is_some(),
            "the command's own confirmation follows the inherited past: {shown:?}"
        );
        assert!(
            position("first answer") < position("rewound to seq"),
            "{shown:?}"
        );

        // Coming back to the child through `/resume` shows the same past.
        let parent_id = session.session_id;
        loop_state
            .dispatch_slash(&format!("/resume {parent_id}"))
            .expect("resume parent");
        assert_eq!(loop_state.session_id, parent_id);
        loop_state
            .dispatch_slash(&format!("/resume {child_id}"))
            .expect("resume child");
        assert_eq!(loop_state.session_id, child_id);
        let shown: Vec<String> = loop_state
            .ui
            .transcript()
            .iter()
            .map(|entry| format!("{entry:?}"))
            .collect();
        assert!(
            shown.iter().any(|line| line.contains("first answer"))
                && !shown.iter().any(|line| line.contains("second")),
            "{shown:?}"
        );

        // A grandchild inherits through both forks.
        loop_state.dispatch_slash("/fork").expect("fork");
        let grandchild = loop_state.session_id;
        assert_ne!(grandchild, child_id);
        let shown: Vec<String> = loop_state
            .ui
            .transcript()
            .iter()
            .map(|entry| format!("{entry:?}"))
            .collect();
        assert!(
            shown.iter().any(|line| line.contains("first answer")),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|line| line.contains("second")),
            "{shown:?}"
        );
    }

    #[test]
    fn a_burst_of_events_that_outruns_a_tick_does_not_end_the_session() {
        // The live channel holds 64 events; a tool-heavy turn or a job's
        // output can land more than that between two ticks. That returned
        // `Lagged` from the drain, and the drain returned it as the end of
        // the session. The subscription names the cursor to resume from,
        // and the ledger promises no gaps or duplicates from it.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let mut locals = LoopLocals::for_session(&session);
        let mut loop_state = locals.session_loop(&session, Vec::new());
        let cancel = CancellationToken::new();
        for _ in 0..300 {
            session
                .client
                .append_turn_progress(
                    session.session_id,
                    &session.actor,
                    TraceId::new(),
                    event_ledger::event::EventKind::ContextIndexed,
                    serde_json::json!({}),
                )
                .expect("event");
        }
        let tip = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        for _ in 0..200 {
            loop_state
                .drain()
                .expect("a lagged stream is resumed, not fatal");
            if loop_state.ui.snapshot().map(|s| s.seq()) == Some(tip) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            loop_state.ui.snapshot().map(|s| s.seq()),
            Some(tip),
            "every event reached the projection: {:?}",
            loop_state.ui.protocol_error()
        );
        assert!(!loop_state.ui.actions_blocked());
    }

    #[test]
    fn a_long_parent_is_inherited_to_its_newest_turns_not_its_oldest() {
        // The replay bound that keeps a resumed session's first paint quick
        // was the wrong bound for inheritance: the transcript projection
        // keeps its newest entries, so folding only the parent's first N
        // events showed a rewound session its oldest turns while the ones
        // just before the rewind point went missing — silently. The fold
        // now reaches the fork point, and a ceiling it cannot reach is said
        // in the transcript rather than swallowed.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn("the oldest turn", ScriptedModel::terminal("oldest answer"));
        for _ in 0..30 {
            session
                .client
                .append_turn_progress(
                    session.session_id,
                    &session.actor,
                    TraceId::new(),
                    event_ledger::event::EventKind::ContextIndexed,
                    serde_json::json!({}),
                )
                .expect("filler event");
        }
        session.run_turn("the newest turn", ScriptedModel::terminal("newest answer"));
        let cancel = CancellationToken::new();
        let tip = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        let child = block_on(
            session.client.fork_session(ForkSession::new(
                session.session_id,
                tip,
                session.actor.clone(),
                TraceId::new(),
            )),
            &cancel,
        )
        .expect("fork");

        // The production ceiling: everything through the fork point.
        let inherited =
            inherited_transcript(&session.client, child.id(), &cancel, MAX_INHERIT_DEPTH);
        let shown: Vec<String> = inherited.iter().map(|e| format!("{e:?}")).collect();
        assert!(
            shown.iter().any(|line| line.contains("oldest answer"))
                && shown.iter().any(|line| line.contains("newest answer")),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|line| line.contains("fully read")),
            "{shown:?}"
        );

        // A ceiling the parent exceeds (the old replay bound, in
        // miniature): the fold stops short and says so, instead of
        // presenting the oldest turns as the whole past.
        let truncated = inherited_transcript_bounded(
            &session.client,
            child.id(),
            &cancel,
            MAX_INHERIT_DEPTH,
            12,
        );
        let shown: Vec<String> = truncated.iter().map(|e| format!("{e:?}")).collect();
        assert!(
            !shown.iter().any(|line| line.contains("newest answer")),
            "{shown:?}"
        );
        assert!(
            shown.iter().any(|line| line.contains("was not fully read")),
            "{shown:?}"
        );
    }

    #[test]
    fn a_turn_threads_warnings_reach_the_transcript_not_the_alt_screen() {
        // A turn's warnings — a reminder roster that does not parse, an MCP
        // server that will not start, no model configured — used to be raw
        // stderr writes, which the alt screen turns into a staircase. They
        // are the user's to read, so they go in the transcript.
        let env = TempEnv::create();
        let root = fs::canonicalize(&env.project).expect("canonicalize");
        fs::create_dir_all(root.join(PROJECT_MARKER)).expect("marker");
        fs::write(
            root.join(PROJECT_MARKER).join("reminders.toml"),
            "this is not = toml [[",
        )
        .expect("broken roster");
        let session = ScriptedSession::create(&env);
        let mut locals = LoopLocals::for_session(&session);
        let mut loop_state =
            locals.session_loop(&session, vec![ScriptedModel::terminal("ran anyway")]);
        loop_state.submit_turn("hello").expect("submit");
        for _ in 0..300 {
            loop_state.drain().expect("drain");
            if !loop_state.model_busy()
                && command_outputs(loop_state.ui)
                    .iter()
                    .any(|t| t.starts_with("warning: reminders not loaded"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let outputs = command_outputs(loop_state.ui);
        assert!(
            outputs
                .iter()
                .any(|t| t.starts_with("warning: reminders not loaded")),
            "{outputs:?}"
        );
        settle_last_turn(&mut loop_state);
        assert!(
            loop_state.ui.transcript().iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Assistant { text } if text == "ran anyway"
            )),
            "the turn still ran: {:?}",
            loop_state.ui.transcript()
        );
    }

    #[test]
    fn forking_moves_the_session_onto_the_child() {
        // `/fork` created a real, durable child and then stayed on the
        // parent: the id and the subscribed stream both still pointed at it,
        // so reducing the child snapshot in *looked* like a switch for one
        // frame and the next drain overwrote it. A branch exists to be
        // worked in.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let parent_id = session.session_id;

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(parent_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(parent_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let mut renderer = TuiRenderer::new(true);
        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scripted_backing_queue(Vec::new()),
        );

        loop_state.dispatch_slash("/fork").expect("fork");
        let child_id = loop_state.session_id;
        assert_ne!(
            child_id, parent_id,
            "the session must be on the child, not the parent it forked from"
        );

        // The switch must survive a drain. Asserting that alone is not
        // enough: with an idle parent a stale subscription delivers nothing
        // and the revert cannot be observed. So the *parent* emits after the
        // fork — a subscription still pointing at it would feed that event
        // into a session it does not belong to.
        // The observable consequence of a half-switch is not that the parent
        // leaks in — `drain` reconciles by id, so it does not — but that the
        // *child's* own events never arrive, because the subscription is
        // still tailing the parent. So the child emits, and the UI must see
        // it.
        session
            .client
            .append_turn_progress(
                child_id,
                &session.actor,
                TraceId::new(),
                event_ledger::event::EventKind::ContextCompiled,
                serde_json::json!({"included_tokens": 7, "context_limit": 9}),
            )
            .expect("the child accepts its own events");
        // The subscription is worker-fed, so one drain can run before the
        // event is queued — the same race that made `replay_history` truncate
        // transcripts. Drain repeatedly so a stale stream has every chance to
        // deliver what it should not have.
        for _ in 0..30 {
            loop_state.drain().expect("drain");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            loop_state.session_id, child_id,
            "and must still be there after the next drain"
        );
        assert_eq!(
            loop_state.ui.snapshot().map(|s| s.id()),
            Some(child_id),
            "the projected snapshot must be the child's, not the parent's"
        );
        assert!(
            !loop_state.ui.actions_blocked(),
            "the switched session must be usable: {:?}",
            loop_state.ui.protocol_error()
        );
        assert_eq!(
            loop_state.ui.context_usage(),
            Some((7, 9)),
            "the child's own events must reach the UI — a subscription left on \
the parent delivers nothing for the session the user is now in"
        );
    }

    #[test]
    fn resume_returns_to_the_session_you_left() {
        // `/resume [session]` was refused as "cross-process session resume
        // is not wired yet" while `switch_to_session` — the exact mechanism,
        // built so `/fork` could move onto its child — sat beside it, and
        // the fork message told the user to leave the TUI and run `rapid
        // resume` to get back. The round trip is the test: fork, then
        // `/resume <parent>`, then a bare `/resume` back to the child.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let parent_id = session.session_id;

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(parent_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(parent_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let mut renderer = TuiRenderer::new(true);
        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scripted_backing_queue(Vec::new()),
        );

        loop_state.dispatch_slash("/fork").expect("fork");
        let child_id = loop_state.session_id;
        assert_ne!(child_id, parent_id);

        // Named: back to the parent, through the real parse -> dispatch ->
        // kernel-action path.
        loop_state
            .dispatch_slash(&format!("/resume {parent_id}"))
            .expect("resume");
        assert_eq!(
            loop_state.session_id, parent_id,
            "naming the parent must move the session back onto it"
        );
        assert_eq!(
            loop_state.ui.snapshot().map(|s| s.id()),
            Some(parent_id),
            "and the projection must be the parent's"
        );
        // The switch must be complete, not cosmetic: the parent's own events
        // must reach the UI, which a subscription left on the child would
        // never deliver.
        session
            .client
            .append_turn_progress(
                parent_id,
                &session.actor,
                TraceId::new(),
                event_ledger::event::EventKind::ContextCompiled,
                serde_json::json!({"included_tokens": 3, "context_limit": 9}),
            )
            .expect("the parent accepts its own events");
        for _ in 0..30 {
            loop_state.drain().expect("drain");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            loop_state.ui.context_usage(),
            Some((3, 9)),
            "the resumed session's events must reach the UI"
        );
        assert!(
            !loop_state.ui.actions_blocked(),
            "{:?}",
            loop_state.ui.protocol_error()
        );

        // Bare: "the other one" — the most recently active session that is
        // not this one. Here that is the child, the only other session.
        loop_state.dispatch_slash("/resume").expect("resume");
        assert_eq!(
            loop_state.session_id, child_id,
            "a bare /resume must move to the other session"
        );
    }

    #[test]
    fn resuming_an_unknown_session_refuses_without_leaving_the_current_one() {
        // `switch_to_session` would fail generically *after* replacing the
        // stream, leaving the user on a session with a dead subscription.
        // The id is checked against the ledger first, and the refusal
        // carries the same hint `rapid resume` prints.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let here = session.session_id;

        let cancel = CancellationToken::new();
        let snapshot = block_on(session.client.get_session(here), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(here, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = reduce(AppState::new(), &UiEvent::Snapshot(snapshot));
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let mut renderer = TuiRenderer::new(true);
        let mut loop_state = autonomous_session_loop(
            &session,
            &mut stream,
            &mut ui,
            &cancel,
            &mut interrupt_count,
            &mut saw_ctrl_c,
            &mut renderer,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scripted_backing_queue(Vec::new()),
        );

        loop_state
            .dispatch_slash("/resume 019c0000-0000-7000-8000-00000000dead")
            .expect("dispatch");
        assert_eq!(
            loop_state.session_id, here,
            "an unknown id must not move the session"
        );
        let text = loop_state
            .ui
            .transcript()
            .iter()
            .map(|entry| format!("{entry:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("no session 019c0000-0000-7000-8000-00000000dead in this project"),
            "the refusal must name the id: {text}"
        );
        assert!(
            text.contains(&here.to_string()),
            "and list the sessions this project does have: {text}"
        );

        // The session is still live: its own events still arrive.
        session
            .client
            .append_turn_progress(
                here,
                &session.actor,
                TraceId::new(),
                event_ledger::event::EventKind::ContextCompiled,
                serde_json::json!({"included_tokens": 5, "context_limit": 9}),
            )
            .expect("append");
        for _ in 0..30 {
            loop_state.drain().expect("drain");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(loop_state.ui.context_usage(), Some((5, 9)));

        // And with only this session recorded, a bare /resume says so
        // rather than "resuming" the session it is already on.
        loop_state.dispatch_slash("/resume").expect("dispatch");
        assert_eq!(loop_state.session_id, here);
    }

    #[test]
    fn the_diff_panel_shows_real_hunks_of_what_the_agent_wrote() {
        // `/diff` showed line counts because nothing in the tree computed a
        // line diff and `+n/-m` from a net change would have claimed a
        // computation that did not happen. The computation happens now, at
        // the write site where both sides are already in memory, and the
        // ledger carries the bounded hunk text — never a copy of the file.
        let env = TempEnv::create();
        std::fs::write(
            env.project.join("lib.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\nfn five() {}\n",
        )
        .expect("seed");
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "rename three",
            ScriptedModel::write_then_answer(
                "lib.rs",
                "fn one() {}\nfn two() {}\nfn drei() {}\nfn four() {}\nfn five() {}\n",
                "renamed",
            ),
        );
        let changed = session.state().changed_files();
        let file = changed
            .get("lib.rs")
            .expect("the written file is projected");
        let hunks = file
            .hunks
            .as_deref()
            .expect("a text-to-text write must carry hunks");
        assert!(
            hunks.contains("-fn three() {}") && hunks.contains("+fn drei() {}"),
            "the hunk must show the line that changed, both sides: {hunks}"
        );
        assert!(
            hunks.contains(" fn two() {}") && hunks.contains(" fn four() {}"),
            "with context around it: {hunks}"
        );
        assert!(
            !hunks.contains("fn one()") || hunks.matches('\n').count() <= 8,
            "and only the surrounding context, not the whole file: {hunks}"
        );

        // Through the real panel, which paints the hunks under the row.
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Diff,
            session.state(),
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted[0].contains("lib.rs") && painted[0].contains("5 -> 5 lines"),
            "the row keeps its counts: {painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("-fn three() {}"))
                && painted.iter().any(|line| line.contains("+fn drei() {}")),
            "the panel must paint the hunk: {painted:?}"
        );

        // A second write replaces the hunks with its own and says so.
        session.run_turn(
            "add six",
            ScriptedModel::write_then_answer(
                "lib.rs",
                "fn one() {}\nfn two() {}\nfn drei() {}\nfn four() {}\nfn five() {}\nfn six() {}\n",
                "added",
            ),
        );
        let changed = session.state().changed_files();
        let file = changed.get("lib.rs").expect("row");
        let hunks = file.hunks.as_deref().expect("hunks");
        // `fn drei` is context for this hunk now; the previous write's
        // *change* to it must not be.
        assert!(
            hunks.contains("+fn six() {}")
                && !hunks.contains("+fn drei")
                && !hunks.contains("-fn three"),
            "the latest write's hunks, not the previous write's: {hunks}"
        );
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Diff,
            session.state(),
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted[0].contains("2 writes, latest shown"),
            "a rewritten file says which write is shown: {painted:?}"
        );
    }

    #[test]
    fn a_turn_s_workspace_writes_reach_the_diff_panel() {
        // `/diff` opened an empty panel for the most basic question a coding
        // CLI answers: what did the agent change? `apps/rapid` writes through
        // `atomic_write` and never touched the `workspace` crate's journal,
        // so nothing recorded a mutation for the panel to project.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        assert!(
            session.state().changed_files().is_empty(),
            "a session that has written nothing has nothing to show"
        );

        session.run_turn(
            "write a note",
            ScriptedModel::write_then_answer("notes.md", "hello", "wrote it"),
        );

        let changed = session.state().changed_files();
        let file = changed
            .get("notes.md")
            .unwrap_or_else(|| panic!("the written file must be listed: {changed:?}"));
        assert_eq!(
            file.lines_before, None,
            "a file that did not exist is new, not a change from zero lines"
        );
        assert_eq!(file.lines_after, 1);
        assert_eq!(file.writes, 1);

        // Writing it again updates the same row rather than adding a second,
        // and the *original* before-state is what the session started from.
        session.run_turn(
            "extend it",
            ScriptedModel::write_then_answer("notes.md", "hello\nagain", "extended"),
        );
        let changed = session.state().changed_files();
        assert_eq!(changed.len(), 1, "one row per file: {changed:?}");
        let file = &changed["notes.md"];
        assert_eq!(
            file.lines_before, None,
            "the first write's before-state is the session's starting point, \
not this session's own earlier output"
        );
        assert_eq!(file.lines_after, 2);
        assert_eq!(file.writes, 2);

        let panel = tui::sidebar_lines(
            tui::state::UiRoute::Diff,
            session.state(),
            70,
            8,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            panel[0].contains("notes.md") && panel[0].contains("new, 2 lines"),
            "the panel must name the file and what became of it: {panel:?}"
        );
        assert!(
            panel[0].contains("2 writes"),
            "a file rewritten twice is a different situation from one touched once: {panel:?}"
        );
    }

    #[test]
    fn a_finished_turn_reports_the_context_it_actually_used() {
        // The `ctx:` item read a dash for every session: the compiler
        // computes `included_tokens` against `context_limit` on every turn
        // (`TokenPartitions`) and nothing carried it out of the host.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        assert_eq!(
            session.state().context_usage(),
            None,
            "before a turn there is no honest figure to show"
        );

        session.run_turn("do something", ScriptedModel::terminal("done"));

        let (used, limit) = session
            .state()
            .context_usage()
            .expect("a finished turn must report what it compiled");
        assert!(used > 0, "a real turn compiles a non-empty context");
        assert!(
            used <= limit,
            "usage must be reported against the limit it was compiled under: {used}/{limit}"
        );
        assert_eq!(
            u32::try_from(limit).expect("limit fits"),
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            "and the limit must be the one this turn actually ran with"
        );

        // And the breakdown, which is what a nearly-full window needs: the
        // totals say how full, the classes say what filled it.
        let partitions = session.state().context_partitions();
        assert!(
            !partitions.is_empty(),
            "a finished turn must report which classes consumed the window"
        );
        assert!(
            partitions.iter().any(|p| p.class == "system" && p.used > 0),
            "the system prompt alone is never zero tokens: {partitions:?}"
        );
        assert!(
            partitions.iter().map(|p| p.used).sum::<u64>() <= used,
            "no class may claim more than the whole compiled context: {partitions:?}"
        );
        let panel = tui::sidebar_lines(
            tui::state::UiRoute::Context,
            session.state(),
            60,
            12,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            panel[0].contains(&format!("{used}/{limit}")),
            "the panel leads with the totals: {panel:?}"
        );
        assert!(
            panel.iter().any(|line| line.contains("system")),
            "and lists the classes beneath them: {panel:?}"
        );

        // It reaches the status line, which is where the dash was.
        let rendered = tui::render_status_with(session.state(), &tui::StatusChrome::default(), 120);
        assert!(
            rendered.content().contains(&format!("{used}")),
            "the status line must show the compiled usage: {}",
            rendered.content()
        );
    }

    #[test]
    fn the_status_bar_shows_the_model_and_policy_this_session_resolved() {
        // Every frame passed `StatusChrome::default()`, so the bar read
        // `model:-  sandbox:-  policy:-  ctx:-` for the whole session: a
        // complete widget fed nothing, on the one surface always on screen.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(
            env.options_capturing_render(vec![InteractiveInput::Submit("/quit".to_owned())]),
        )
        .expect("run");
        let painted = report
            .rendered_output
            .expect("capture_render was requested");

        // Only the policy item is asserted: the model shown is whatever the
        // *process environment* configures, which is a property of the
        // machine running the test, not of this code. The permission mode is
        // resolved for every session either way.
        //
        // Either label style counts — the bar compacts `policy:` to `pol:`
        // when a long model name crowds the line, which is the widget's own
        // fitting behaviour and not something this test should pin.
        assert!(
            !painted.contains("policy:-") && !painted.contains("pol:-"),
            "the bar must show the mode that governs tool calls:\n{painted}"
        );
        assert!(
            painted.contains("policy:ask") || painted.contains("pol:ask"),
            "an unconfigured project runs in default mode, which asks:\n{painted}"
        );
    }

    #[test]
    fn the_policy_item_reads_the_mode_table_rather_than_the_mode_s_name() {
        // `dontAsk` **denies**; a name-based guess reads it as "allow
        // without prompting", which would tell a user their session is
        // permissive when it refuses everything. Every mapping here is the
        // decision `PermissionLattice`'s own mode table returns.
        use crate::permissions::PermissionMode as Mode;
        assert_eq!(policy_mode_for(Mode::Default), tui::PolicyMode::Ask);
        assert_eq!(policy_mode_for(Mode::Plan), tui::PolicyMode::Deny);
        assert_eq!(policy_mode_for(Mode::DontAsk), tui::PolicyMode::Deny);
        assert_eq!(
            policy_mode_for(Mode::BypassPermissions),
            tui::PolicyMode::Allow
        );
        // Mixed modes report the more permissive of the two answers they
        // give: overstating permissiveness keeps a user cautious, while
        // understating it would not.
        assert_eq!(policy_mode_for(Mode::AcceptEdits), tui::PolicyMode::Allow);
        assert_eq!(policy_mode_for(Mode::Auto), tui::PolicyMode::Allow);
    }

    #[test]
    fn the_status_bar_never_claims_a_sandbox_or_context_it_cannot_confirm() {
        // Both are deliberately left unset: `shell_exec` takes `sandbox` per
        // *call*, so a session-wide posture would tell a user their commands
        // are confined when most are not; and nothing projects the compiled
        // context size, so a window with a zero `used` would be wrong the
        // moment a turn ran. The dash is the honest reading.
        let chrome = session_status_chrome(crate::permissions::PermissionMode::Default);
        assert_eq!(chrome.sandbox(), tui::SandboxMode::Unknown);
        assert_eq!(chrome.context(), tui::ContextUsage::default());
    }

    #[test]
    fn jobs_cancel_stops_a_running_background_job() {
        // `/jobs cancel` was refused outright — "no per-job cancellation
        // backend exists yet" — and that was true while the table lived one
        // turn: by the time a user could type it, there was nothing left to
        // cancel. With a session-scoped table there is, and the mechanism is
        // the `cancelled` flag `kill_all` already sets.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start a slow one",
            ScriptedModel::background_job_then_answer(&["/bin/sleep", "30"], "started"),
        );
        assert_eq!(
            session.state().jobs().values().next().expect("job").state(),
            tui::state::JobLifecycle::Started,
        );

        // Through the real registry the session holds, exactly as
        // `SessionLoop::cancel_job` does.
        assert_eq!(
            session.jobs.cancel(None),
            Some(1),
            "the running job must be the one cancelled"
        );
        session.drain_until("the cancelled job to be reported", |state| {
            state
                .jobs()
                .values()
                .any(|job| !matches!(job.state(), tui::state::JobLifecycle::Started))
        });
        let jobs = session.state().jobs();
        let job = jobs.values().next().expect("job");
        assert_eq!(
            job.state(),
            tui::state::JobLifecycle::Cancelled,
            "and it must be reported as cancelled: {job:?}"
        );

        // Cancelling again reports honestly rather than claiming a second
        // success — the distinction `cancel_job`'s own message relies on.
        assert_eq!(session.jobs.cancel(None), Some(0));
    }

    #[test]
    fn cancelling_an_unknown_job_is_not_reported_as_success() {
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        assert_eq!(
            session.jobs.cancel(Some(protocol::JobId::new())),
            None,
            "an id this session never had must be distinguishable from one that \
was already finished"
        );
        assert_eq!(
            session.jobs.cancel(None),
            Some(0),
            "and a session with no jobs cancels nothing rather than erroring"
        );
    }

    #[test]
    fn a_background_job_outlives_the_turn_that_started_it() {
        // The point of `background: true`: start a build, keep working, come
        // back to it. The registry used to be built per turn and its `Drop`
        // killed the children, so a job was dead before the user could type
        // anything — `/jobs` is only reachable *between* turns, so nothing a
        // user could do would ever have shown a live one.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start a slow one",
            ScriptedModel::background_job_then_answer(&["/bin/sleep", "30"], "started"),
        );

        let jobs = session.state().jobs();
        let job = jobs.values().next().expect("the job is projected");
        assert_eq!(
            job.state(),
            tui::state::JobLifecycle::Started,
            "the job must still be running after its turn ended: {job:?}"
        );

        // A second turn runs, and it is still there — same table, same job.
        session.run_turn(
            "keep working",
            ScriptedModel::terminal("did something else"),
        );
        let jobs = session.state().jobs();
        assert_eq!(jobs.len(), 1, "the same job, not a second one: {jobs:?}");
        let job = jobs.values().next().expect("the job");
        assert_eq!(
            job.state(),
            tui::state::JobLifecycle::Started,
            "and still running a turn later: {job:?}"
        );
        assert_eq!(job.command(), Some("/bin/sleep 30"));
    }

    #[test]
    fn a_job_stopped_when_the_session_ends_says_so_rather_than_reporting_an_exit_code() {
        // Ending the session is what stops background jobs now, and that is
        // the guarantee that keeps no command outliving the CLI. The *report*
        // used to be wrong either way: `kill_all` sets `cancelled` and then
        // kills, so the supervisor saw an ordinary exit and recorded
        // "completed exit -1" — which a model reads as a build that failed,
        // and the panel showed the same way.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start a slow one",
            ScriptedModel::background_job_then_answer(&["/bin/sleep", "30"], "started"),
        );
        assert_eq!(
            session
                .state()
                .jobs()
                .values()
                .next()
                .expect("the job")
                .state(),
            tui::state::JobLifecycle::Started,
        );

        // End the session: the last handle to the table goes, and its `Drop`
        // stops every child.
        session.end_session_jobs();
        session.drain_until("the stopped job to be reported", |state| {
            state
                .jobs()
                .values()
                .any(|job| !matches!(job.state(), tui::state::JobLifecycle::Started))
        });

        let jobs = session.state().jobs();
        let job = jobs.values().next().expect("the job is projected");
        assert_eq!(
            job.state(),
            tui::state::JobLifecycle::Cancelled,
            "a job stopped with the session must be reported as cancelled, not as an exit: {job:?}"
        );
        assert_eq!(
            job.exit_status(),
            None,
            "and must not carry an exit status it never really had"
        );
    }

    #[test]
    fn a_background_job_reaches_the_jobs_panel() {
        // `shell_exec` with `background: true` has always started a real
        // supervised child, but the registry is in-process and journaled
        // nothing, so `/jobs` — which projects `job.*` ledger events — was
        // permanently empty for a feature that was working the whole time.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start the build",
            ScriptedModel::background_job_then_answer(&["/bin/echo", "building"], "started it"),
        );

        // `job.started` is appended by the tool call itself, so it is in the
        // ledger by the time the turn completes.
        let jobs = session.state().jobs();
        assert_eq!(jobs.len(), 1, "the started job must be projected: {jobs:?}");
        let job = jobs.values().next().expect("one job");
        assert_eq!(
            job.command(),
            Some("/bin/echo building"),
            "the panel needs to say what is running, not just a uuid"
        );
        // Completion is reported by the job's own supervisor thread, which
        // can land before or after the turn returns — so wait for the real
        // event rather than asserting on whichever happened to win.
        session.drain_until("the job to complete", |state| {
            state
                .jobs()
                .values()
                .all(|job| matches!(job.state(), tui::state::JobLifecycle::Completed))
        });
        let jobs = session.state().jobs();
        let job = jobs.values().next().expect("one job");
        assert_eq!(
            job.exit_status(),
            Some(0),
            "a completed job must carry how it exited: {job:?}"
        );

        // And it renders, through the real panel.
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            session.state(),
            60,
            6,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted[0].contains("/bin/echo building"),
            "the jobs panel must show the command: {painted:?}"
        );
    }

    #[test]
    fn jobs_logs_shows_the_output_the_job_actually_produced() {
        // `/jobs logs <id>` parsed its id from the beginning and every
        // consumer dropped it at `Inspector::route`, whose `UiRoute` has no
        // room for "which one" — so it opened the same unfiltered list as a
        // bare `/jobs`. The bytes were being spooled the whole time: the
        // `job_output` tool hands them to the *model*, and the user's own
        // advertised command was the only reader with no path to them.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start the build",
            // The output must be a value the *command text* does not
            // contain. A first version ran `/bin/echo hello-from-the-job`
            // and asserted the panel showed "hello-from-the-job" — which
            // the list view already paints as the job's command, so the
            // test passed with the whole feature reverted. The product of
            // the two operands appears only in what the process printed.
            ScriptedModel::background_job_then_answer(
                &["/bin/sh", "-c", "echo $((123456789 * 2))"],
                "started it",
            ),
        );
        // The spool is filled by the job's own supervisor thread, so wait
        // for the real terminal state rather than racing it.
        session.drain_until("the job to complete", |state| {
            state
                .jobs()
                .values()
                .all(|job| matches!(job.state(), tui::state::JobLifecycle::Completed))
        });
        let job_id = *session.state().jobs().keys().next().expect("one job");

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        // The projection the turn already built — the panel needs the job
        // rows, and the registry behind `session.jobs` holds the bytes.
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        // The real production path: parse -> dispatch -> focus -> sync.
        loop_state
            .dispatch_slash(&format!("/jobs logs {job_id}"))
            .expect("dispatch");

        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            60,
            10,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted.iter().any(|line| line.contains("246913578")),
            "the logs view must show what the job printed: {painted:?}"
        );

        // And it is the logs view specifically that shows it: a bare
        // `/jobs` clears the page rather than leaving it painted.
        loop_state.dispatch_slash("/jobs").expect("dispatch");
        let listed = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            60,
            10,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            !listed.iter().any(|line| line.contains("246913578")),
            "leaving the logs view must stop painting its output: {listed:?}"
        );
    }

    #[test]
    fn jobs_show_narrows_the_panel_to_the_job_that_was_named() {
        // The same dropped operand, at the other resolution: with two jobs
        // running, `/jobs show <id>` painted both rows exactly like a bare
        // `/jobs`, so naming one job had no observable effect at all.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start the first",
            ScriptedModel::background_job_then_answer(&["/bin/echo", "first-job"], "ok"),
        );
        session.run_turn(
            "start the second",
            ScriptedModel::background_job_then_answer(&["/bin/echo", "second-job"], "ok"),
        );
        let jobs = session.state().jobs().clone();
        assert_eq!(jobs.len(), 2, "two jobs must be projected: {jobs:?}");
        let (wanted, wanted_command) = jobs
            .iter()
            .find_map(|(id, job)| {
                job.command()
                    .filter(|command| command.contains("second-job"))
                    .map(|command| (*id, command.to_owned()))
            })
            .expect("the second job is projected with its command");

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        loop_state
            .dispatch_slash(&format!("/jobs show {wanted}"))
            .expect("dispatch");
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            80,
            10,
            &tui::state::CancellationToken::new(),
        );
        let body = painted.join("\n");
        assert!(
            body.contains(&wanted_command),
            "the named job must be shown: {painted:?}"
        );
        assert!(
            !body.contains("first-job"),
            "naming one job must not paint the other: {painted:?}"
        );
    }

    #[test]
    fn a_bare_jobs_logs_reads_the_most_recent_job() {
        // Nothing shows a `JobId`: the panel paints a job's *command* when
        // the producer recorded one, exactly so a reader can tell which row
        // is the test run they are waiting on. So a `/jobs logs` that only
        // answers to a UUID is answerable only by someone who already has
        // one. Bare means the newest job.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start the first",
            ScriptedModel::background_job_then_answer(
                &["/bin/sh", "-c", "echo $((111 * 3))"],
                "ok",
            ),
        );
        session.run_turn(
            "start the second",
            ScriptedModel::background_job_then_answer(
                &["/bin/sh", "-c", "echo $((222 * 3))"],
                "ok",
            ),
        );
        session.drain_until("both jobs to finish", |state| {
            state.jobs().len() == 2 && state.jobs().values().all(|job| job.state().is_terminal())
        });

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        loop_state.dispatch_slash("/jobs logs").expect("dispatch");
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            70,
            10,
            &tui::state::CancellationToken::new(),
        );
        let body = painted.join("\n");
        assert!(
            body.contains("666"),
            "a bare `/jobs logs` must read the most recently started job: {painted:?}"
        );
        assert!(
            !body.contains("333"),
            "and not an older one's output: {painted:?}"
        );
    }

    #[test]
    fn an_open_logs_view_shows_the_lines_a_job_wrote_just_before_it_stopped() {
        // Open the view while the job is running, then look away until the
        // job has written its last line and exited. The first tick after
        // that sees the job stop; the last line is in the spool and nowhere
        // else, and a view that stops re-reading a finished job would keep
        // showing the page it opened with. CI's Linux runner made exactly
        // this happen: the job's exit and its last line inside one tick.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        // The job writes its last line only once the test says so, by
        // creating this file — a fixed sleep let a slow runner reach the
        // dispatch below after the job had already finished.
        let go = env.project.join("go");
        let script = format!(
            "echo $((1000 + 1)); while [ ! -e '{}' ]; do sleep 0.02; done; echo $((2000 + 2))",
            go.display()
        );
        session.run_turn(
            "start a job that writes once more before it exits",
            ScriptedModel::background_job_then_answer(
                &["/bin/sh", "-c", script.as_str()],
                "started it",
            ),
        );
        session.drain_until("the first line to be spooled", |state| {
            !state.jobs().is_empty()
        });

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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
        loop_state.dispatch_slash("/jobs logs").expect("dispatch");
        let job = loop_state.ui.job_logs().expect("view open").job();
        assert!(
            !loop_state
                .ui
                .job_logs()
                .expect("view")
                .lines()
                .iter()
                .any(|l| l.contains("2002")),
            "the view opened before the last line existed"
        );

        // Let the job finish, and look away: no ticks until the last line
        // is spooled and the job has had time to be recorded as done.
        std::fs::write(&go, b"").expect("release the job");
        let spooled = Instant::now() + Duration::from_secs(10);
        while !loop_state
            .jobs
            .logs(job)
            .is_some_and(|(text, _)| text.contains("2002"))
        {
            assert!(
                Instant::now() < spooled,
                "the job never wrote its last line"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(
            crate::exec_tools::JOB_POLL_INTERVAL * 3 + crate::exec_tools::JOB_OUTPUT_SETTLE,
        );

        // Now tick until the projection sees the job stop; on that tick the
        // view must already carry the last line.
        let terminal_seen = Instant::now() + Duration::from_secs(10);
        loop {
            loop_state.drain().expect("drain");
            let terminal = loop_state
                .ui
                .jobs()
                .get(&job)
                .is_some_and(|projected| projected.state().is_terminal());
            if terminal {
                break;
            }
            assert!(Instant::now() < terminal_seen, "the job never stopped");
            std::thread::sleep(Duration::from_millis(20));
        }
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted.iter().any(|line| line.contains("2002")),
            "the tick that saw the job stop must show what it wrote last: {painted:?}"
        );
        assert!(
            loop_state.ui.job_logs().expect("view").is_complete(),
            "a page read after the job stopped is final"
        );
    }

    #[test]
    fn an_open_logs_view_follows_a_job_that_is_still_writing() {
        // The reason to open a build log is to watch it. A page captured
        // once at dispatch would freeze under a header that still says the
        // job is running, so the view is re-read from the same spool
        // `job_output` serves the model from.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start a job that keeps writing",
            ScriptedModel::background_job_then_answer(
                &[
                    "/bin/sh",
                    "-c",
                    "echo $((1000 + 1)); sleep 1; echo $((2000 + 2))",
                ],
                "started it",
            ),
        );
        // Wait for the first line only — the job is deliberately still
        // running at this point.
        session.drain_until("the first line to be spooled", |state| {
            !state.jobs().is_empty()
        });

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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
        loop_state.dispatch_slash("/jobs logs").expect("dispatch");

        // The second line is written a second later, by the job's own
        // process — so drive the real drain loop in real time and require
        // the open view to pick it up without the command being re-run.
        let mut followed = false;
        for _ in 0..200 {
            loop_state.drain().expect("drain");
            let painted = tui::sidebar_lines(
                tui::state::UiRoute::Jobs,
                loop_state.ui,
                70,
                12,
                &tui::state::CancellationToken::new(),
            );
            if painted.iter().any(|line| line.contains("2002")) {
                followed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            followed,
            "an open logs view must follow output written after it was opened"
        );
    }

    #[test]
    fn agents_show_selects_the_agent_that_was_named() {
        // The same dropped operand as `/jobs show`, at the same boundary.
        // The agents panel has resolved `AppState::selected_agent` into its
        // selected row since it existed — `resolve_selection` falls back to
        // the caller's row index only when the field is unset — and the
        // panel paints a `>` marker plus that row's detail block. Nothing
        // ever wrote the field, so `/agents show <id>` parsed an id and
        // then described whichever agent happened to sort first.
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        let named = "019c0000-0000-7000-8000-0000000000a7";
        loop_state
            .dispatch_slash(&format!("/agents show {named}"))
            .expect("dispatch");
        assert_eq!(
            loop_state.ui.selected_agent().map(|id| id.to_string()),
            Some(named.to_owned()),
            "the agent named on the command line must become the selected one"
        );

        // And opening the list again clears it, so the panel goes back to
        // describing nothing in particular rather than keeping a stale
        // agent selected.
        loop_state.dispatch_slash("/agents").expect("dispatch");
        assert_eq!(
            loop_state.ui.selected_agent(),
            None,
            "the list view must not keep the previous selection"
        );
    }

    #[test]
    fn context_search_runs_the_retrieval_the_turn_path_would_run() {
        // `/context search <query>` parsed a query and dropped it at
        // `Inspector::route`, opening the same compiled-context summary as
        // a bare `/context` — a search that never searched. The backend was
        // there the whole time: `context_retrieval::retrieve` is what every
        // trusted turn already calls to pick proactive context.
        let env = TempEnv::create();
        std::fs::write(
            env.project.join("lru.py"),
            "class LRUCache:\n    def get(self, key):\n        return self._data.get(key)\n",
        )
        .expect("seed");
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        loop_state
            .dispatch_slash("/context search how does LRUCache eviction work")
            .expect("dispatch");
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Context,
            loop_state.ui,
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        let body = painted.join("\n");
        assert!(
            body.contains("lru.py"),
            "the search must name the file retrieval found: {painted:?}"
        );

        // And leaving the search restores the compiled-context summary
        // rather than leaving stale results painted.
        loop_state.dispatch_slash("/context").expect("dispatch");
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Context,
            loop_state.ui,
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            !painted.iter().any(|line| line.contains("lru.py")),
            "leaving the search must stop painting its results: {painted:?}"
        );
    }

    #[test]
    fn context_search_never_indexes_an_untrusted_project() {
        // Retrieval walks the tree and writes an incremental index under
        // `.rapidlm/index/`, which is why the turn path runs it only for a
        // trusted project. A slash command must not be a way around that:
        // typing `/context search` in an untrusted directory would
        // otherwise index it on the user's behalf.
        let env = TempEnv::create();
        std::fs::write(
            env.project.join("lru.py"),
            "class LRUCache:\n    def get(self, key):\n        return self._data.get(key)\n",
        )
        .expect("seed");
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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
        loop_state.trusted = false;

        loop_state
            .dispatch_slash("/context search how does LRUCache eviction work")
            .expect("dispatch");
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Context,
            loop_state.ui,
            70,
            12,
            &tui::state::CancellationToken::new(),
        );
        let body = painted.join("\n");
        assert!(
            body.contains("not trusted"),
            "an untrusted project must say why it found nothing: {painted:?}"
        );
        assert!(
            !body.contains("lru.py"),
            "and must not have read the tree: {painted:?}"
        );
        assert!(
            !env.project.join(".rapidlm/index").exists(),
            "an untrusted project must not be indexed by a slash command"
        );
    }

    #[test]
    fn diff_agent_says_it_cannot_narrow_rather_than_pretending_to() {
        // The last operand in the family, and the only one that cannot be
        // honored: `workspace.mutation_detected` records a path and line
        // counts and no agent, and subagents write through the parent
        // turn's sink. Silently painting every change under a flag that
        // asked for one agent's is the failure mode this whole family was
        // about — so it says what it is doing instead.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit(
                "/diff --agent 019c0000-0000-7000-8000-0000000000a7".to_owned(),
            ),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("no per-agent attribution"),
            "a flag that cannot narrow anything must say so:\n{painted}"
        );

        // And a bare `/diff` says nothing of the kind — the notice belongs
        // to the flag, not to the panel.
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/diff".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            !painted.contains("no per-agent attribution"),
            "a bare /diff must not carry the flag's notice:\n{painted}"
        );
    }

    #[test]
    fn jobs_cancel_accepts_the_handle_the_panel_shows() {
        // The id `/jobs cancel <id>` demanded was a 36-character UUID that
        // nothing displayed: the panel paints a job's *command* so a reader
        // can tell which is the test run, and the transcript calls it
        // `job-1`. A command answerable only by an input the product never
        // shows is not much better than a broken one. The handle has been
        // in the `job.started` payload since the producer existed — its own
        // comment says it is there so a reader can correlate the row with
        // the transcript — and the projection dropped it.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "start a slow one",
            ScriptedModel::background_job_then_answer(&["/bin/sleep", "30"], "started"),
        );
        let job = session.state().jobs().values().next().expect("job").clone();
        assert_eq!(job.state(), tui::state::JobLifecycle::Started);
        let handle = job
            .handle()
            .expect("the projection must keep the handle the producer recorded")
            .to_owned();
        assert!(
            handle.starts_with("job-"),
            "the model-facing handle: {handle}"
        );

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(Vec::new());
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

        // The panel shows the handle, so the handle must be accepted —
        // through the real parse -> dispatch -> cancel path.
        let painted = tui::sidebar_lines(
            tui::state::UiRoute::Jobs,
            loop_state.ui,
            70,
            6,
            &tui::state::CancellationToken::new(),
        );
        assert!(
            painted[0].contains(&handle),
            "the panel must show the name the command accepts: {painted:?}"
        );
        loop_state
            .dispatch_slash(&format!("/jobs cancel {handle}"))
            .expect("dispatch");
        assert_eq!(
            *loop_state.interrupt_count, 0,
            "naming a job must never become a session interrupt"
        );

        // And the tail the panel would show without a handle resolves too.
        let full = job.id().to_string();
        let tail = full.rsplit('-').next().expect("uuid has dash groups");
        let parsed = tui::parse_command_in(&format!("/jobs show {tail}"), loop_state.ui)
            .expect("the id's tail must parse");
        assert_eq!(
            parsed,
            tui::UiCommand::JobsShow { id: Some(job.id()) },
            "the tail must resolve to the same job"
        );

        // Cancellation actually happened, as the supervisor reports it.
        session.drain_until("the cancelled job to be reported", |state| {
            state
                .jobs()
                .values()
                .any(|job| matches!(job.state(), tui::state::JobLifecycle::Cancelled))
        });
    }

    #[test]
    fn every_doc_the_binary_points_a_user_at_exists() {
        // `NOT_CONFIGURED_HINT` told a user with no model configured to
        // "see docs/configuration.md" — a file that does not exist. The
        // first thing a new user reads was a dead end. Every user-facing
        // string that names a `docs/` path is gathered here and each path
        // checked against the repository, so a moved or renamed document
        // fails a test instead of a first run.
        let mut texts: Vec<String> = vec![
            NOT_CONFIGURED_HINT.to_owned(),
            CLI_USAGE.clone(),
            RESUME_USAGE.to_owned(),
            crate::command_help::annotated_catalog_help(),
        ];
        for entry in tui::catalog() {
            if let Ok(command) = tui::parse_command(&format!("/{}", entry.name)) {
                match tui::dispatch(command) {
                    tui::FrontendAction::Kernel(action) => {
                        texts.push(unsupported_command_text(&action));
                    }
                    tui::FrontendAction::Local(tui::LocalAction::Open(inspector)) => {
                        texts.push(unrouted_inspector_text(&inspector));
                    }
                    _ => {}
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");
        let mut named = std::collections::BTreeSet::new();
        for text in &texts {
            for token in text.split(|c: char| {
                c.is_whitespace() || c == ';' || c == ',' || c == ')' || c == '(' || c == '`'
            }) {
                if token.starts_with("docs/") && token.ends_with(".md") {
                    named.insert(token.to_owned());
                }
            }
        }
        assert!(
            !named.is_empty(),
            "the binary points users at documentation; this test must be finding those pointers"
        );
        for path in &named {
            assert!(
                root.join(path).is_file(),
                "the binary points a user at `{path}`, which does not exist under {}",
                root.display()
            );
        }
    }

    #[test]
    fn the_most_recent_session_is_chosen_by_activity_not_creation() {
        // `rapid resume` with no id means "where I left off". A session
        // created yesterday and worked in today is the one a user means, so
        // the default reads `last_activity`, not creation order.
        //
        // "Activity" is deliberately what was *recorded*, not what was
        // opened: resuming a session to look at it writes nothing to the
        // ledger, so reading history never rewrites it.
        let env = TempEnv::create();
        let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
        assert!(
            most_recent_session(&ledger_path)
                .expect("no ledger is not an error")
                .is_none(),
            "a project with no ledger has no session to resume"
        );

        let mut older = ScriptedSession::create(&env);
        older.run_turn("first", ScriptedModel::terminal("in the older session"));
        let mut newer = ScriptedSession::create(&env);
        newer.run_turn("second", ScriptedModel::terminal("in the newer session"));
        assert_ne!(older.session_id, newer.session_id);
        assert_eq!(
            most_recent_session(&ledger_path).expect("summaries"),
            Some(newer.session_id),
            "with both freshly worked in, the newest activity wins"
        );

        // Now work in the *older* session again: it becomes the one to
        // resume, even though the other was created later.
        older.run_turn("back to the first", ScriptedModel::terminal("later work"));
        assert_eq!(
            most_recent_session(&ledger_path).expect("summaries"),
            Some(older.session_id),
            "the session most recently worked in must win over the one created last"
        );
    }

    /// Sessions recorded in a project's ledger.
    fn recorded_sessions(ledger_path: &Path) -> usize {
        event_ledger::ledger::EventLedger::open(ledger_path)
            .expect("ledger")
            .list_sessions(&event_ledger::ledger::CancellationToken::new())
            .expect("summaries")
            .len()
    }

    #[test]
    fn bare_help_marks_the_commands_this_build_cannot_perform() {
        // `/help` listed 28 families with nothing to say that about half
        // report "not available" the moment they run. `/help <command>`
        // keeps its own usage text unchanged.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/help".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("run a marked command"),
            "bare /help must say where the specific reason lives:\n{painted}"
        );
        // Specific commands with their specific markers, not just the
        // substring "unavailable" appearing somewhere. Chosen from the tail
        // of the catalog because 28 lines plus a footer overflow a 24-row
        // test terminal and the head scrolls out of the captured frame; the
        // per-command verdicts for the whole catalog are asserted directly
        // in `command_help`'s own `the_rendered_help_marks_only_what_is_
        // missing`.
        let rows = painted_rows(&painted);
        // Marked rows begin with the marker, not the command, so match on
        // the command anywhere in the row and assert the prefix separately.
        let row_for = |command: &str| {
            rows.iter()
                .find(|row| row.contains(command))
                .unwrap_or_else(|| panic!("`{command}` is missing from /help:\n{rows:#?}"))
                .clone()
        };
        // The marker is a leading, fixed-width prefix precisely so it stays
        // visible: several usage lines are long enough that a trailing
        // marker wrapped onto the next terminal row. Commands are chosen
        // from the tail of the catalog because it is longer than a 24-row
        // test terminal and the head scrolls out of the captured frame; the
        // whole catalog's verdicts are asserted directly by `command_help`'s
        // own `the_rendered_help_marks_only_what_is_missing`.
        for command in ["/playbook ", "/plugins "] {
            let row = row_for(command);
            assert!(
                row.trim_start().starts_with('!'),
                "`{command}` should be marked unavailable: {row}"
            );
        }
        let mcp = row_for("/mcp ");
        assert!(
            mcp.trim_start().starts_with('~'),
            "`/mcp` should be marked partly unavailable: {mcp}"
        );
        // A fully working command is unmarked.
        let permissions = row_for("/permissions ");
        assert!(
            permissions.trim_start().starts_with('/'),
            "a fully working command must carry no marker: {permissions}"
        );
    }

    #[test]
    fn help_for_one_command_is_its_own_usage_not_the_annotated_catalog() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/help quit".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            !painted.contains("run a marked command"),
            "per-command help must not become the whole catalog:\n{painted}"
        );
        // And it must actually show the command's own usage — asserting
        // only the footer's absence would pass if it printed nothing at all.
        assert!(
            painted.contains("/quit"),
            "`/help quit` must print `/quit`'s own usage:\n{painted}"
        );
    }

    #[test]
    fn fork_reports_the_branch_it_moved_onto_and_how_to_get_back() {
        // History of this test: `/fork` originally reduced the child snapshot
        // into the UI and printed nothing, so it looked like a switch for one
        // frame and then silently reverted; it was then made to say it had
        // *not* switched; and now it really does switch. What has to stay
        // true throughout is that a user can tell which session they are in
        // and how to reach the other one.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/fork".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("now on child session"),
            "the branch it moved onto must be named:\n{painted}"
        );
        // In-TUI, not `rapid resume`: this used to point out of the TUI
        // for something the TUI now does itself.
        assert!(
            painted.contains("/resume"),
            "and the way back to the parent must be given, since the session \
the user was in is no longer the one they are in:\n{painted}"
        );
        // The reported session is the child, not the parent it forked from.
        let parent = report.session_id.expect("a session id");
        assert!(
            !painted.contains(&format!("now on child session {parent}")),
            "the child must be a different session than the one that forked:\n{painted}"
        );
    }

    #[test]
    fn the_session_still_works_after_a_rewind() {
        // `/rewind <seq>` swaps in a prefix projection — the kernel replays
        // events 1..seq into a snapshot and mutates nothing. The ledger and
        // the live subscription stay at the tip. So the *next* event the
        // session records has a seq far past the projection's, and the
        // reducer's `apply_next` demands exactly `snapshot.seq + 1`. Only
        // the error path of rewind was tested; this is the path a user
        // actually takes — rewind, then keep working.
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        session.run_turn(
            "first",
            ScriptedModel::terminal_with_usage("first answer", 5, 1),
        );
        let tip = session.state().snapshot().map(|s| s.seq()).expect("a tip");
        assert!(
            tip > 2,
            "the first turn must have recorded events: tip={tip}"
        );

        let cancel = CancellationToken::new();
        let snapshot =
            block_on(session.client.get_session(session.session_id), &cancel).expect("session");
        let mut stream = block_on(
            session
                .client
                .subscribe(SubscribeEvents::new(session.session_id, snapshot.seq())),
            &cancel,
        )
        .expect("subscribe");
        let mut ui = session.state().clone();
        let mut interrupt_count = 0u32;
        let mut saw_ctrl_c = false;
        let turn_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut renderer = TuiRenderer::new(true);
        let backings = scripted_backing_queue(vec![ScriptedModel::terminal_with_usage(
            "second answer",
            5,
            1,
        )]);
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

        let parent_id = loop_state.session_id;
        loop_state.dispatch_slash("/rewind 1").expect("rewind");
        // A rewind is a fork at the sequence plus the switch `/fork` makes:
        // the session is now a child whose tip *is* the rewound sequence,
        // so the projection, the stream and the kernel agree.
        assert_ne!(
            loop_state.session_id, parent_id,
            "a rewind moves onto a branch rather than mutating history"
        );
        assert_eq!(
            loop_state.ui.snapshot().map(|s| s.seq()),
            Some(1),
            "the projection is at the rewound sequence"
        );
        assert!(
            !loop_state.ui.actions_blocked(),
            "{:?}",
            loop_state.ui.protocol_error()
        );

        // Now keep working: a real turn through the real submit path.
        loop_state.submit_turn("second").expect("submit");
        for _ in 0..400 {
            loop_state.drain().expect("drain");
            if !turn_in_flight.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..30 {
            loop_state.drain().expect("drain");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !loop_state.ui.actions_blocked(),
            "a turn after a rewind must not freeze the session: {:?}",
            loop_state.ui.protocol_error()
        );
        let transcript = loop_state
            .ui
            .transcript()
            .iter()
            .map(|entry| format!("{entry:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            transcript.contains("second answer"),
            "the turn after the rewind must reach the transcript: {transcript}"
        );
    }

    #[test]
    fn a_rewind_to_an_impossible_sequence_is_a_command_error_not_the_end_of_the_session() {
        // `block_on(...)?` propagated the kernel's rejection out of
        // `dispatch_slash` -> `submit_composer` -> `run`, ending the whole
        // interactive session. Sequence 0 and any sequence past the
        // session's last are the *ordinary* mistakes on this command, and
        // `command_error_text`'s own doc comment describes fixing exactly
        // this failure mode for parse errors — the kernel-action path still
        // had it.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/rewind 999999".to_owned()),
            // Reached only if the session survived the line above.
            InteractiveInput::Submit("/permissions allow repo_read".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("the session must survive a rejected rewind");
        assert_eq!(
            report.outcome,
            InteractiveOutcome::Quit,
            "a rejected rewind must not end the session"
        );
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("rewind to 999999 failed"),
            "the rejection must be reported as a command error:\n{painted}"
        );
        // Proof the session really kept going: the command after it ran.
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        assert_eq!(
            persisted_grants_for(&canonical, &env.user_home)
                .iter()
                .map(crate::permissions::ToolPattern::render)
                .collect::<Vec<_>>(),
            vec!["repo_read".to_owned()],
            "the command after the failed rewind never ran, so the session did end"
        );
    }

    #[test]
    fn cancelling_a_named_job_does_not_silently_interrupt_the_turn_instead() {
        // The shape of the original defect: `/jobs cancel <id>` reached
        // `KernelApi::Interrupt`, whose only implementation is a session-wide
        // interrupt taking no id, so it killed whatever was running and
        // reported nothing about the job the user named. The command has a
        // real backend now, but the property this test exists for is
        // unchanged — naming a job must never become "interrupt the
        // session" — and it holds for an id that does not exist either.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit(
                "/jobs cancel 01234567-89ab-7cde-89ab-0123456789ab".to_owned(),
            ),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert_eq!(
            report.interrupt_count, 0,
            "the session was interrupted by a command that names a job"
        );
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("no job 01234567-89ab-7cde-89ab-0123456789ab in this session"),
            "an id this session never had must be said so, not acknowledged as \
cancelled and not turned into a turn interrupt:\n{painted}"
        );
    }

    #[test]
    fn a_bare_cancel_names_its_own_target_and_interrupts_nothing() {
        // `/jobs cancel` and `/agents cancel` once routed to a session-wide
        // interrupt under commands summarised as job and agent control —
        // the wrong thing, done silently. Each now answers from its own
        // table: with nothing running, it says so, and the turn is never
        // interrupted.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/jobs cancel".to_owned()),
            InteractiveInput::Submit("/agents cancel".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert_eq!(
            report.interrupt_count, 0,
            "a bare cancel must not interrupt the turn either"
        );
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("no running jobs to cancel"), "{painted}");
        assert!(painted.contains("no running agents to cancel"), "{painted}");
        assert!(!painted.contains("not available"), "{painted}");
    }

    #[test]
    fn permissions_slash_command_grants_through_the_real_store() {
        // Option C's whole point: a user who sees a call denied can approve
        // it from inside the session, and the next run really allows it.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/permissions allow workspace_write".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("allow=workspace_write"), "{painted}");

        // Read back the way a real run does: the production reader, against
        // the home this session used.
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        let grants = persisted_grants_for(&canonical, &env.user_home);
        assert_eq!(
            grants
                .iter()
                .map(crate::permissions::ToolPattern::render)
                .collect::<Vec<_>>(),
            vec!["workspace_write".to_owned()],
            "the slash command did not reach the store a real run reads"
        );
    }

    #[test]
    fn permissions_slash_command_accepts_a_pattern_with_a_glob() {
        // A composer line is split on whitespace, and the glob half is
        // routinely written with spaces (`shell_exec(git *)`), so the
        // operand is rejoined rather than requiring one shell word.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("/permissions allow shell_exec(git *)".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        assert_eq!(
            persisted_grants_for(&canonical, &env.user_home)
                .iter()
                .map(crate::permissions::ToolPattern::render)
                .collect::<Vec<_>>(),
            vec!["shell_exec(git *)".to_owned()]
        );
    }

    #[test]
    fn bare_permissions_renders_the_real_grant_list_not_a_no_panel_note() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/permissions allow repo_read".to_owned()),
            InteractiveInput::Submit("/permissions".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("grants=1"), "{painted}");
        assert!(painted.contains("allow=repo_read"), "{painted}");
        assert!(
            !painted.contains("no TUI permission panel"),
            "the route-less placeholder is gone: {painted}"
        );
    }

    #[test]
    fn an_invalid_permission_pattern_is_refused_and_writes_nothing() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/permissions allow not/a/pattern".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("not a valid pattern"), "{painted}");
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        assert!(
            persisted_grants_for(&canonical, &env.user_home).is_empty(),
            "a refused pattern must not be written"
        );
    }

    #[test]
    fn permissions_revoke_removes_what_allow_recorded() {
        let _lock = lock_terminal();
        let env = TempEnv::create();
        let report = run_interactive(env.options(vec![
            InteractiveInput::Submit("/permissions allow workspace_write".to_owned()),
            InteractiveInput::Submit("/permissions revoke workspace_write".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        assert!(persisted_grants_for(&canonical, &env.user_home).is_empty());
    }

    #[test]
    fn mcp_list_renders_the_real_project_report_inline() {
        // `/mcp list` and `/mcp doctor` used to open a routeless inspector,
        // i.e. do nothing. They now print the report `rapid mcp list`
        // prints, from the same loader the turn path registers from.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("marker");
        fs::write(
            env.project.join(PROJECT_MARKER).join("settings.json"),
            r#"{"mcpServers": {"good": {"command": "true"},
                                "remote": {"type": "http", "url": "https://example.com"}}}"#,
        )
        .expect("settings");
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/mcp list".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(painted.contains("servers="), "{painted}");
        assert!(
            painted.contains("good"),
            "the usable server must be listed:\n{painted}"
        );
        // Not `contains("rejected")`: `list` always prints `rejected=<n>`,
        // so that would hold with zero rejections.
        assert!(
            painted.contains("rejected=remote"),
            "the rejected entry must be reported by name:\n{painted}"
        );
        assert!(
            painted.contains("stdio only"),
            "with its real reason:\n{painted}"
        );
    }

    #[test]
    fn mcp_list_reports_trust_from_the_catalog_this_session_actually_read() {
        // The session resolves its RapidLM home from `InteractiveOptions`,
        // not from the process environment. A slash command that re-derived
        // one from `std::env::vars()` would report trust out of
        // `$HOME/.rapidlm` — a catalog this session never consulted, and on
        // a developer machine very likely a *different* answer.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("marker");
        fs::write(
            env.project.join(PROJECT_MARKER).join("settings.json"),
            r#"{"mcpServers": {"srv": {"command": "true"}}}"#,
        )
        .expect("settings");
        // Grant in *this session's* home only.
        let canonical = fs::canonicalize(&env.project).expect("canonicalize");
        let identity = ProjectIdentity::new(&canonical, None).expect("identity");
        ProjectTrustStore::open(env.user_home.join(TRUST_CATALOG_NAME))
            .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
            .expect("grant");

        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/mcp list".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("trust=trusted"),
            "the grant in this session's own home was not observed:\n{painted}"
        );
    }

    #[test]
    fn mcp_remove_from_the_tui_respects_the_approval_classification_and_writes_nothing() {
        // `rapid mcp remove` exists and `/mcp remove <name>` fits its
        // grammar exactly, so wiring it was tempting — but
        // `KernelAction::requires_approval` classifies every MCP mutation as
        // approval-gated and this build has no approval broker. Wiring it
        // would have made it the first approval-classified action in the
        // binary that silently mutates the filesystem, and it deletes from
        // `.claude/settings.json`, a file another tool owns, from a two-word
        // slash command with no confirmation.
        let _lock = lock_terminal();
        let env = TempEnv::create();
        fs::create_dir_all(env.project.join(PROJECT_MARKER)).expect("marker");
        let settings = env.project.join(PROJECT_MARKER).join("settings.json");
        let before = r#"{"mcpServers": {"gone": {"command": "true"}}}"#;
        fs::write(&settings, before).expect("settings");
        let report = run_interactive(env.options_capturing_render(vec![
            InteractiveInput::Submit("/mcp remove gone".to_owned()),
            InteractiveInput::Submit("/quit".to_owned()),
        ]))
        .expect("run");
        assert_eq!(report.outcome, InteractiveOutcome::Quit);
        assert_eq!(
            fs::read_to_string(&settings).expect("settings still readable"),
            before,
            "an approval-gated action mutated project settings with no approval"
        );
        let painted = report
            .rendered_output
            .expect("capture_render was requested");
        assert!(
            painted.contains("rapid mcp remove"),
            "it must name the argv-only command that can do this:\n{painted}"
        );
    }

    #[test]
    fn mcp_add_names_the_command_that_can_actually_do_it() {
        // `/mcp add <target>` has nowhere to put a program and its
        // arguments, so it stays unsupported — but the message must send the
        // user to `rapid mcp add`, which now exists, rather than claim MCP
        // management is unwired anywhere.
        let text = unsupported_command_text(&KernelAction::AddMcp {
            target: "x".to_owned(),
        });
        assert!(text.contains("rapid mcp add"), "{text}");
        let auth = unsupported_command_text(&KernelAction::AuthMcp {
            name: "x".to_owned(),
        });
        assert!(auth.contains("stdio"), "{auth}");
        assert_ne!(text, auth, "add and auth fail for different reasons");
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
        assert_eq!(
            command_error_text(&CommandError::TooLong),
            "command too long"
        );
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
        assert!(mcp_text.contains("rapid mcp add"), "{mcp_text}");
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
        let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
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
        /// Set via `capturing_blocks`: every block of every `step()` call,
        /// as `(locator, text)`, so a test can assert on what the compiled
        /// context carried — the session's earlier turns, say.
        captured_blocks: Option<CapturedBlocks>,
    }

    /// `(locator, text)` of every block a `ScriptedModel` was handed.
    type CapturedBlocks = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

    impl ScriptedModel {
        fn capturing_blocks(mut self, sink: CapturedBlocks) -> Self {
            self.captured_blocks = Some(sink);
            self
        }

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

        /// Call `tool` with `arguments` (a JSON object), then answer.
        fn call_then_answer(tool: &str, arguments: serde_json::Value, answer: &str) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                tool,
                serde_json::to_string(&arguments).expect("encode args"),
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

        /// Refuse the first packet as too large, answer the recovery's
        /// compaction request with `summary`, then answer the retried step.
        fn overflow_then_summary_then_answer(summary: &str, answer: &str) -> Self {
            Self {
                outputs: VecDeque::from(vec![
                    Err(ModelStepError::BoundExceeded),
                    Ok(ModelStepOutput::Terminal {
                        text: summary.to_owned(),
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

        /// Start a background `shell_exec` job, then answer — the shape a
        /// model uses to kick off a long build and keep working.
        fn background_job_then_answer(argv: &[&str], answer: &str) -> Self {
            let argv_json = serde_json::to_string(argv).expect("argv");
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::SHELL_EXEC_TOOL,
                format!(r#"{{"argv":{argv_json},"background":true}}"#),
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

        fn write_then_answer(path: &str, content: &str, answer: &str) -> Self {
            // Serialized, not interpolated: a `content` containing a
            // newline or a quote produced invalid JSON and surfaced as an
            // opaque `InvalidToolCall` from the tool layer, which reads like
            // a production bug rather than a broken fixture.
            let call = ProposedToolCall::new(
                "c1",
                crate::exec_tools::WORKSPACE_WRITE_TOOL,
                serde_json::to_string(&serde_json::json!({
                    "path": path,
                    "content": content,
                }))
                .expect("encode write args"),
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
            if let Some(sink) = &self.captured_blocks {
                let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                for block in blocks {
                    sink.push((block.locator().to_owned(), block.text().to_owned()));
                }
            }
            // Deterministic, not incidental: without this, whether a
            // scripted turn's measured `active_ms` reads as nonzero would
            // depend on how fast the surrounding context/redaction-registry
            // setup happens to run on whatever machine executes the test —
            // real work today, but not something a test should rely on
            // staying slow enough to round up to a whole millisecond.
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
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
        /// The session's job table, exactly as `run_started_session` owns
        /// one — so a scripted turn's background jobs behave the way a real
        /// session's do, including outliving the turn that started them.
        jobs: crate::exec_tools::JobRegistry,
        /// What the session's threads share — notices, MCP connections —
        /// as `run_started_session` owns one for the whole session.
        shared: SessionShared,
        session_id: protocol::SessionId,
        actor: ActorRef,
        root: PathBuf,
        /// Mirrors `ResolvedProject::user_home`: the home a real session
        /// resolved, so a scripted `SessionLoop` reads the same trust
        /// catalog production would.
        user_home: PathBuf,
        stream: EventStream,
        ui: AppState,
    }

    impl ScriptedSession {
        fn create(env: &TempEnv) -> Self {
            let ledger_path = project_ledger_path(&env.project.join(PROJECT_MARKER));
            fs::create_dir_all(ledger_path.parent().expect("ledger has a parent"))
                .expect("ledger dir");
            let client = InProcessKernelClient::open(&ledger_path).expect("open ledger");
            let actor = human_actor().expect("actor");
            let cancel = CancellationToken::new();
            let snapshot = block_on(
                client.create_session(CreateSession::new(
                    ProjectId::new(),
                    actor.clone(),
                    TraceId::new(),
                )),
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
                jobs: crate::exec_tools::JobRegistry::default(),
                shared: SessionShared::default(),
                session_id,
                actor,
                root: env.project.clone(),
                user_home: env.user_home.clone(),
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
                self.jobs.clone(),
                self.shared.clone(),
            );
            join.join()
                .expect("the turn thread must not panic (catching_panics wraps its body)");
            assert!(
                !turn_in_flight.load(std::sync::atomic::Ordering::SeqCst),
                "turn_in_flight must reset to false once the turn thread finishes"
            );
            let snapshot =
                block_on(self.client.get_session(self.session_id), &cancel).expect("session");
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

        /// Drop this session's job-table handle, as leaving
        /// `run_started_session` does — the moment background jobs are
        /// stopped.
        fn end_session_jobs(&mut self) {
            self.jobs = crate::exec_tools::JobRegistry::default();
        }

        /// Keep draining the real subscription until `done` holds or the
        /// budget runs out. For facts a *background* thread produces after
        /// the turn returns — a job's completion — where asserting
        /// immediately would race the supervisor's own poll interval.
        fn drain_until(&mut self, what: &str, done: impl Fn(&AppState) -> bool) {
            let cancel = CancellationToken::new();
            for _ in 0..200 {
                drain_kernel_events(
                    &self.client,
                    &mut self.stream,
                    &mut self.ui,
                    self.session_id,
                    &cancel,
                )
                .expect("drain");
                if done(&self.ui) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for {what}");
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
    fn the_denial_reason_survives_the_ledger_payload_bridge() {
        // The last link in the chain that carries a refusal's reason to the
        // user: `agent-runtime` puts it on `TurnEvent::ToolDenied`, this sink
        // must put it in the ledger payload, and `tui::state`'s fold reads it
        // back under the key written here. Each of the other links has its
        // own test; without this one, dropping the field here would be
        // invisible.
        const REASON: &str = "workspace_write denied: requires approval; \
pre-approve it with `rapid permissions allow <tool>`";
        let env = TempEnv::create();
        let session = ScriptedSession::create(&env);
        let cancel = CancellationToken::new();
        let expected_seq = block_on(session.client.get_session(session.session_id), &cancel)
            .expect("session")
            .seq();
        let handle = block_on(
            session.client.submit_turn(SubmitTurn::new(
                session.session_id,
                expected_seq,
                session.actor.clone(),
                TraceId::new(),
                "denied turn",
            )),
            &cancel,
        )
        .expect("submit turn");

        {
            let mut sink = InteractiveTurnSink {
                client: &session.client,
                session_id: session.session_id,
                actor: &session.actor,
            };
            agent_runtime::TurnEventSink::emit(
                &mut sink,
                agent_runtime::TurnEvent::ToolDenied {
                    turn_id: handle.turn_id(),
                    call_id: "c1".to_owned(),
                    tool: crate::exec_tools::WORKSPACE_WRITE_TOOL.to_owned(),
                    reason: Some(REASON.to_owned()),
                },
            )
            .expect("emit");
        }

        let events = session
            .client
            .export_events(session.session_id, &CancellationToken::new())
            .expect("export");
        let denial = events
            .iter()
            .find(|event| event.kind == event_ledger::event::EventKind::ToolDenied.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "a ToolDenied event was recorded: {:?}",
                    events.iter().map(|e| &e.kind).collect::<Vec<_>>()
                )
            });
        let payload: serde_json::Value =
            serde_json::from_str(&denial.payload_json).expect("payload json");
        assert_eq!(
            payload.get("detail").and_then(serde_json::Value::as_str),
            Some(REASON),
            "the reason was dropped on the way to the ledger: {payload}"
        );
        // Under the key `tui::state`'s fold actually reads.
        assert!(payload.get("tool").is_some());
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
                TranscriptEntry::ToolActivity { tool, status: ToolActivityStatus::Completed, .. }
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
            ScriptedModel::terminal("ok")
                .capturing_system_prompt(std::sync::Arc::clone(&small_captured)),
            (2_000, 200),
        );
        let large_captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.run_turn_with_budget(
            "say hi again",
            ScriptedModel::terminal("ok")
                .capturing_system_prompt(std::sync::Arc::clone(&large_captured)),
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
        assert!(
            usage.active_ms() > 0,
            "active_ms must reflect real elapsed time, got 0"
        );
    }

    #[test]
    fn a_turn_with_no_active_goal_does_not_create_or_affect_one() {
        let env = TempEnv::create();
        let mut session = ScriptedSession::create(&env);
        // Deliberately no `create_active_goal()` call.

        session.run_turn(
            "no goal here",
            ScriptedModel::terminal_with_usage("done", 100, 100),
        );

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
        assert_eq!(
            usage.turns(),
            1,
            "a failed turn still counts as one incurred turn"
        );
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
        assert_eq!(
            usage.turns(),
            1,
            "the turn still ran (and failed), so it still counts"
        );
        assert_eq!(
            usage.tokens(),
            0,
            "must not invent tokens that were never reported"
        );
        assert_eq!(
            usage.cost(),
            0,
            "must not invent cost that was never reported"
        );
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

        session.run_turn(
            "first",
            ScriptedModel::terminal_with_usage("first done", 100, 10),
        );
        session.run_turn(
            "second",
            ScriptedModel::terminal_with_usage("second done", 250, 40),
        );

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
            !transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnFailed { .. })),
            "needing context is not a failure and must not show a failure banner: {transcript:?}"
        );
        assert!(
            !transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::TurnInterrupted)),
            "needing context is not a cancellation: {transcript:?}"
        );
        assert!(
            !transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity {
                    status: ToolActivityStatus::Failed,
                    ..
                }
            )),
            "needing context must not show a failure marker on the ask_user call either: \
             {transcript:?}"
        );
        assert!(
            transcript.iter().any(|entry| matches!(
                entry,
                TranscriptEntry::ToolActivity {
                    status: ToolActivityStatus::ContextRequired,
                    ..
                }
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
            occurrences,
            1,
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
        assert_eq!(
            usage.turns(),
            1,
            "a context-required turn still counts as one incurred turn"
        );
        assert_eq!(
            usage.tokens(),
            1,
            "ScriptedModel::asks_for_context reports 1 token"
        );
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
                TranscriptEntry::ToolActivity { tool, status: ToolActivityStatus::Completed, .. }
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
            Some(agent_runtime::TurnFailureDetail::new(
                "shell_exec",
                "not found",
            )),
        );
        assert_eq!(context_required_question(&with_tool_failed), None);

        let with_no_reason = test_exec_outcome(None, None);
        assert_eq!(context_required_question(&with_no_reason), None);

        let with_context_required = test_exec_outcome(
            Some(agent_runtime::TurnStopReason::ContextRequired),
            Some(agent_runtime::TurnFailureDetail::new(
                "ask_user",
                "Which one?",
            )),
        );
        assert_eq!(
            context_required_question(&with_context_required),
            Some("Which one?")
        );
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
            report
                .summary
                .contains("Which environment should I deploy to?"),
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
            recovered: None,
            context_tokens: None,
            context_partitions: Vec::new(),
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
            user_home: &session.user_home,
            trusted: true,
            turn_in_flight,
            jobs: crate::exec_tools::JobRegistry::default(),
            renderer: &mut renderer,
            autonomous: None,
            compaction: None,
            shared: SessionShared::default(),
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
            resume: None,
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
            &UiEvent::Local(LocalUiEvent::SetViewport {
                width: 120,
                height: 30,
            }),
        );
        renderer.render(&wide).expect("render wide");

        let narrow = reduce(
            session.state().clone(),
            &UiEvent::Local(LocalUiEvent::SetViewport {
                width: 40,
                height: 10,
            }),
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
            renderer
                .transcript
                .push_entry(&tui::state::TranscriptEntry::Assistant {
                    text: format!("line {i}"),
                });
        }
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SetViewport {
                width: 80,
                height: 8,
            }),
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
            renderer
                .transcript
                .push_entry(&tui::state::TranscriptEntry::Assistant {
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
                user_home: &session.user_home,
                trusted: true,
                turn_in_flight: turn_in_flight.clone(),
                jobs: crate::exec_tools::JobRegistry::default(),
                renderer: &mut renderer,
                autonomous: None,
                compaction: None,
                shared: SessionShared::default(),
                #[cfg(test)]
                scripted_backings: None,
            };
            loop_state
                .handle_input(InteractiveInput::PageUp)
                .expect("page up");
        }
        assert!(
            !renderer.viewport.follow_tail(),
            "PageUp must reach the renderer's viewport"
        );

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
                user_home: &session.user_home,
                trusted: true,
                turn_in_flight,
                jobs: crate::exec_tools::JobRegistry::default(),
                renderer: &mut renderer,
                autonomous: None,
                compaction: None,
                shared: SessionShared::default(),
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
            InteractiveInput::Resize {
                width: 80,
                height: 24,
            },
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
