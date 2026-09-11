//! Slash-command palette and typed frontend action router.
//!
//! Composer text that starts with `/` is parsed into [`UiCommand`]. Dispatch
//! maps those values to local chrome or [`KernelAction`]. The router never
//! treats input as a shell string and never applies policy-gated work locally.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;
use std::sync::LazyLock;

use protocol::{AgentId, IdParseError, JobId, KnowledgeId, SessionId};

use crate::sanitize::sanitize_untrusted;
use crate::state::UiRoute;

/// Maximum UTF-8 bytes accepted by [`parse_command`].
pub const MAX_COMMAND_BYTES: usize = 4096;

/// Maximum UTF-8 bytes retained for a free-text argument.
pub const MAX_ARG_BYTES: usize = 1024;

/// Maximum palette rows returned by [`suggest`].
pub const MAX_PALETTE_RESULTS: usize = 16;

/// Typed slash-command result. Not a shell invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiCommand {
    Help {
        topic: Option<String>,
    },
    Quit,
    GoalShow,
    GoalStart {
        statement: String,
    },
    GoalPause,
    GoalResume,
    GoalCancel,
    GoalBudget {
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
    },
    /// Start (or resume, if one was already in progress) autonomous
    /// continuation on the active goal. Explicit-only: an active goal never
    /// runs autonomously on its own — see `KernelAction::RunGoal`'s own doc
    /// comment for why.
    GoalRun,
    /// Request the running autonomous driver to stop after its current
    /// iteration (or immediately, if none is in flight). Does not pause or
    /// cancel the goal itself — only the autonomous loop.
    GoalStop,
    AgentsList,
    AgentsShow {
        id: Option<AgentId>,
    },
    AgentsPause {
        id: Option<AgentId>,
    },
    AgentsResume {
        id: Option<AgentId>,
    },
    AgentsSleep {
        id: Option<AgentId>,
    },
    AgentsCancel {
        id: Option<AgentId>,
    },
    AgentsTerminate {
        id: Option<AgentId>,
    },
    OpenDiff {
        agent: Option<AgentId>,
    },
    OpenMemory,
    OpenPermissions,
    /// Grant control plane. Not a [`KernelAction`], and deliberately not
    /// approval-gated: a grant *is* the user's approval, so requiring an
    /// approval to record one would be circular. This is the same reason
    /// `rapid trust grant` is not gated either — and the opposite of
    /// `/mcp remove`, which asks the agent's control plane to mutate
    /// project configuration and therefore stays gated.
    PermissionsAllow {
        pattern: String,
    },
    PermissionsRevoke {
        pattern: String,
    },
    ContextStatus,
    ContextSearch {
        query: String,
    },
    ContextReindex,
    ContextInspect,
    KnowledgeList,
    KnowledgeShow {
        id: Option<KnowledgeId>,
    },
    KnowledgeSuggest {
        text: String,
    },
    KnowledgeApprove {
        id: KnowledgeId,
    },
    KnowledgeReject {
        id: KnowledgeId,
    },
    KnowledgeEdit {
        id: KnowledgeId,
    },
    PlaybookList,
    PlaybookShow {
        name: String,
    },
    PlaybookRun {
        name: String,
    },
    PlaybookValidate {
        name: String,
    },
    TraceShow,
    TraceExport,
    InsightsShow,
    InsightsAnalyze,
    InsightsProposals,
    JobsList,
    JobsShow {
        id: Option<JobId>,
    },
    JobsCancel {
        id: Option<JobId>,
    },
    JobsLogs {
        id: Option<JobId>,
    },
    ModelList,
    ModelSelect {
        name: String,
    },
    ModelDoctor,
    McpList,
    McpAdd {
        target: String,
    },
    McpRemove {
        name: String,
    },
    McpAuth {
        name: String,
    },
    McpDoctor,
    PluginList,
    PluginInstall {
        spec: String,
    },
    PluginRemove {
        name: String,
    },
    PluginPermissions {
        name: String,
    },
    PolicyExplain,
    PolicyCheck,
    SandboxDoctor,
    Resume {
        session: Option<SessionId>,
    },
    Fork,
    Rewind {
        to_seq: Option<u64>,
    },
    Compact,
    Apply {
        agent: Option<AgentId>,
    },
    Rollback {
        checkpoint: Option<String>,
    },
    Handoff {
        dest: HandoffDest,
        target: Option<String>,
    },
    Takeover {
        surface: TakeoverSurface,
    },
    ControlReturn,
    ComputerStatus,
    ComputerObserve,
    ComputerRecord,
    ComputerTest,
}

/// Execution-handoff destination. Not a filesystem path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HandoffDest {
    Local,
    Daemon,
    Remote,
}

/// Control-lease surface for takeover.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TakeoverSurface {
    Terminal,
    Browser,
    Desktop,
    Mobile,
}

/// Parse failure. Display never echoes untrusted payload text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Empty,
    TooLong,
    NotACommand,
    UnknownCommand,
    InvalidArgs {
        command: &'static str,
    },
    InvalidId {
        field: &'static str,
    },
    /// A short identifier matched more than one thing the session knows.
    AmbiguousId {
        field: &'static str,
        matched: usize,
    },
}

/// Local chrome or kernel-bound action. Never a shell string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontendAction {
    Local(LocalAction),
    Kernel(KernelAction),
    Quit,
    InlineHelp(InlineHelp),
}

/// View-only intent. Cannot mutate kernel/session domain state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalAction {
    Open(Inspector),
    /// Record or remove a persisted per-project tool grant. Local because it
    /// is a host-level file the frontend owns, not kernel/session state — and
    /// because a grant is the user's own approval, it carries no approval
    /// gate of its own (see [`UiCommand::PermissionsAllow`]).
    Permissions(PermissionsIntent),
}

/// Which way a [`LocalAction::Permissions`] goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionsIntent {
    Allow { pattern: String },
    Revoke { pattern: String },
}

/// Inspector the TUI can focus. Mapping onto [`UiRoute`] is best-effort.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Inspector {
    /// `id` is the agent `/agents show` named.
    ///
    /// Carried for the same reason [`Inspector::Jobs`] carries one: the
    /// panel has resolved `AppState::selected_agent` into its selected row
    /// since it existed (`panels::agents::resolve_selection`), and nothing
    /// ever set the field — so `/agents show <id>` parsed an id, dropped
    /// it, and painted the detail block of whichever agent sorted first.
    Agents {
        id: Option<AgentId>,
    },
    Diff {
        agent: Option<AgentId>,
    },
    Goal,
    /// `query` is the text `/context search` named, if any.
    ///
    /// Carried for the same reason the other selections are: the query was
    /// parsed and then dropped, so `/context search <text>` opened the same
    /// compiled-context summary as a bare `/context` and never searched.
    Context {
        query: Option<String>,
    },
    Memory,
    /// `id` is the job `/jobs show|logs` named, `logs` which view of it.
    ///
    /// Carried rather than dropped: `parse_jobs` has always parsed this id,
    /// and every consumer discarded it at [`Inspector::route`], so `/jobs
    /// show <id>` and `/jobs logs <id>` opened the same unfiltered list as
    /// a bare `/jobs` — a parsed operand with no effect.
    Jobs {
        id: Option<JobId>,
        logs: bool,
    },
    Knowledge,
    Playbook,
    Trace,
    Insights,
    Models,
    Mcp,
    Plugins,
    Policy,
    Sandbox,
    Permissions,
    Computer,
}

/// Side-effecting action that must enter [`kernel::KernelClient`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelAction {
    PauseGoal,
    ResumeGoal,
    CancelGoal,
    StartGoal {
        statement: String,
    },
    ShowGoalBudget {
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
    },
    /// Begin (or continue) autonomous execution on the active goal. Never
    /// triggered by anything but an explicit `/goal run` — activating or
    /// resuming a goal is metadata/lifecycle only and must never implicitly
    /// start spending model usage on its own.
    RunGoal,
    /// Stop the running autonomous driver after its current iteration.
    StopGoal,
    PauseAgent {
        id: Option<AgentId>,
    },
    ResumeAgent {
        id: Option<AgentId>,
    },
    SleepAgent {
        id: Option<AgentId>,
    },
    CancelAgent {
        id: Option<AgentId>,
    },
    TerminateAgent {
        id: Option<AgentId>,
    },
    CancelJob {
        id: Option<JobId>,
    },
    ReindexContext,
    SuggestKnowledge {
        text: String,
    },
    ApproveKnowledge {
        id: KnowledgeId,
    },
    RejectKnowledge {
        id: KnowledgeId,
    },
    EditKnowledge {
        id: KnowledgeId,
    },
    RunPlaybook {
        name: String,
    },
    ValidatePlaybook {
        name: String,
    },
    SelectModel {
        name: String,
    },
    AddMcp {
        target: String,
    },
    RemoveMcp {
        name: String,
    },
    AuthMcp {
        name: String,
    },
    InstallPlugin {
        spec: String,
    },
    RemovePlugin {
        name: String,
    },
    SetPluginPermissions {
        name: String,
    },
    ResumeSession {
        session: Option<SessionId>,
    },
    ForkSession,
    RewindSession {
        to_seq: Option<u64>,
    },
    CompactSession,
    ApplyChangeSet {
        agent: Option<AgentId>,
    },
    Rollback {
        checkpoint: Option<String>,
    },
    Handoff {
        dest: HandoffDest,
        target: Option<String>,
    },
    Takeover {
        surface: TakeoverSurface,
    },
    ControlReturn,
    ComputerObserve,
    ComputerRecord,
    ComputerTest,
}

/// Kernel client entry used for a [`KernelAction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum KernelApi {
    Approve,
    Interrupt,
    ForkSession,
    Rewind,
    SubmitTurn,
    Dispatch,
}

/// Static usage shown inline when parse fails or `/help` is requested.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineHelp {
    topic: Option<String>,
    usage: String,
}

/// One palette row. Labels are static catalog text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaletteEntry {
    name: &'static str,
    usage: &'static str,
    summary: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    summary: &'static str,
}

const CATALOG: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        aliases: &["?"],
        usage: "/help [command]",
        summary: "show slash-command usage",
    },
    CommandSpec {
        name: "quit",
        aliases: &["q"],
        usage: "/quit",
        summary: "leave the interactive session",
    },
    CommandSpec {
        name: "model",
        aliases: &["models"],
        usage: "/model [list|select <name>|doctor]",
        summary: "list or pin an eligible model",
    },
    CommandSpec {
        name: "agents",
        aliases: &[],
        usage: "/agents [list|show|pause|resume|sleep|cancel|terminate] [id]",
        summary: "inspect or control the agent tree",
    },
    CommandSpec {
        name: "goal",
        aliases: &[],
        usage: "/goal [show|start <text>|pause|resume|cancel|budget [turns N] [tokens N]|run|stop]",
        summary: "inspect, control, or autonomously run the active goal",
    },
    CommandSpec {
        name: "diff",
        aliases: &[],
        usage: "/diff [--agent <id>]",
        summary: "open the pending change preview",
    },
    CommandSpec {
        name: "apply",
        aliases: &[],
        usage: "/apply [--agent <id>]",
        summary: "request kernel apply of a change set",
    },
    CommandSpec {
        name: "rollback",
        aliases: &[],
        usage: "/rollback [checkpoint]",
        summary: "request kernel rollback to a checkpoint",
    },
    CommandSpec {
        name: "context",
        aliases: &[],
        usage: "/context [status|search <query>|reindex|inspect]",
        summary: "inspect compiled context",
    },
    CommandSpec {
        name: "memory",
        aliases: &[],
        usage: "/memory",
        summary: "open the memory inspector",
    },
    CommandSpec {
        name: "knowledge",
        aliases: &[],
        usage: "/knowledge [list|show [id]|suggest <text>|approve <id>|reject <id>|edit <id>]",
        summary: "browse or govern knowledge candidates",
    },
    CommandSpec {
        name: "playbook",
        aliases: &[],
        usage: "/playbook [list|show <name>|run <name>|validate <name>]",
        summary: "inspect or request a playbook run",
    },
    CommandSpec {
        name: "trace",
        aliases: &[],
        usage: "/trace [show|export]",
        summary: "open the trace inspector",
    },
    CommandSpec {
        name: "insights",
        aliases: &[],
        usage: "/insights [show|analyze|proposals]",
        summary: "open session insights",
    },
    CommandSpec {
        name: "handoff",
        aliases: &[],
        usage: "/handoff local|daemon|remote <target>",
        summary: "request a fenced execution handoff",
    },
    CommandSpec {
        name: "takeover",
        aliases: &[],
        usage: "/takeover terminal|browser|desktop|mobile",
        summary: "request an exclusive control lease",
    },
    CommandSpec {
        name: "control-return",
        aliases: &[],
        usage: "/control-return",
        summary: "return control after takeover",
    },
    CommandSpec {
        name: "computer",
        aliases: &[],
        usage: "/computer [status|observe|record|test]",
        summary: "inspect or request computer-use actions",
    },
    CommandSpec {
        name: "jobs",
        aliases: &[],
        usage: "/jobs [list|show|cancel|logs] [id]",
        summary: "inspect or cancel supervised jobs",
    },
    CommandSpec {
        name: "mcp",
        aliases: &[],
        usage: "/mcp [list|add <target>|remove <name>|auth <name>|doctor]",
        summary: "inspect or request MCP changes",
    },
    CommandSpec {
        name: "permissions",
        aliases: &[],
        usage: "/permissions [list|allow <pattern>|revoke <pattern>]",
        summary: "list or record this project's tool grants",
    },
    CommandSpec {
        name: "plugins",
        aliases: &[],
        usage: "/plugins [list|install <spec>|remove <name>|permissions <name>]",
        summary: "inspect or request plugin changes",
    },
    CommandSpec {
        name: "policy",
        aliases: &[],
        usage: "/policy [explain|check]",
        summary: "explain the effective policy",
    },
    CommandSpec {
        name: "sandbox",
        aliases: &[],
        usage: "/sandbox [doctor]",
        summary: "open sandbox diagnostics",
    },
    CommandSpec {
        name: "resume",
        aliases: &[],
        usage: "/resume [session]",
        summary: "resume a session through the kernel",
    },
    CommandSpec {
        name: "fork",
        aliases: &[],
        usage: "/fork",
        summary: "branch a child session at this point (this session stays on the parent)",
    },
    CommandSpec {
        name: "rewind",
        aliases: &[],
        usage: "/rewind <seq>",
        summary: "rewind the session through the kernel",
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        usage: "/compact",
        summary: "request kernel transcript compaction",
    },
];

/// One catalog entry, exposed so a frontend can reason about the command
/// surface it renders — in particular whether this build can actually perform
/// each command — without a second copy of the list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandInfo {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
}

/// Every slash command, in the order bare `/help` lists them.
pub fn catalog() -> impl Iterator<Item = CommandInfo> {
    CATALOG.iter().map(|spec| CommandInfo {
        name: spec.name,
        usage: spec.usage,
        summary: spec.summary,
    })
}

/// The full slash-command catalog, one `usage` line per [`CommandSpec`].
///
/// Derived from [`CATALOG`] rather than written out beside it. The two were
/// separate lists and had already drifted for four commands — `/goal`,
/// `/knowledge`, `/mcp` and `/plugins` each showed a *more precise* usage
/// from `/help <name>` (which reads `CATALOG`) than from bare `/help` (which
/// read the literal), so the catalog listing quietly under-documented four
/// commands' arguments. The test that was supposed to catch that compared
/// `const GOLDEN_HELP: &str = CATALOG_HELP;` against `CATALOG_HELP`, which
/// is a tautology and could never fail.
static CATALOG_HELP: LazyLock<String> = LazyLock::new(|| {
    let mut text = String::new();
    for (index, spec) in CATALOG.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(spec.usage);
    }
    text
});

/// [`CATALOG_HELP`] as a `&'static str`. Sound because the `LazyLock` is a
/// `static`, so the `String` it yields lives for the program.
fn catalog_help() -> &'static str {
    CATALOG_HELP.as_str()
}

/// Parse a composer line into a typed command.
///
/// Input that does not start with `/` is [`CommandError::NotACommand`] so the
/// caller can submit it as a prompt. Unknown names and invalid arguments
/// return [`CommandError`] with [`CommandError::help`].
/// Fewest characters a short identifier may have before it is looked up.
///
/// Below this, `/jobs cancel a` would match whichever job happens to end in
/// `a`; an accidental keystroke should be a parse error, not a cancellation.
pub const MIN_SHORT_ID_CHARS: usize = 4;

/// How a short identifier resolved against what the session knows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution<T> {
    Found(T),
    /// More than one candidate matched; `matched` says how many.
    Ambiguous {
        matched: usize,
    },
    Unknown,
}

/// Resolves the short identifiers panels display back to the typed ids
/// commands need.
///
/// The rule this exists to keep: **a panel must never show an identifier
/// the commands refuse.** `JobId` and `AgentId` are UUIDv7 — time-ordered,
/// so ids minted seconds apart share their first ten hex characters and a
/// git-style *prefix* would rarely disambiguate anything. What the panels
/// show instead is the last dash-group, the random tail, and that is what
/// this resolves: a full UUID always parses, and otherwise the text is
/// matched as a suffix of the ids the session knows (plus, for jobs, the
/// `job-N` handle the transcript uses). A unique match resolves; several
/// is [`Resolution::Ambiguous`]; none is [`Resolution::Unknown`].
pub trait IdResolver {
    fn resolve_job(&self, text: &str) -> Resolution<JobId>;
    fn resolve_agent(&self, text: &str) -> Resolution<AgentId>;
}

/// Resolves nothing: full UUIDs only. What [`parse_command`] uses, so a
/// parse with no session behind it behaves exactly as it always did.
pub struct NoResolver;

impl IdResolver for NoResolver {
    fn resolve_job(&self, _: &str) -> Resolution<JobId> {
        Resolution::Unknown
    }

    fn resolve_agent(&self, _: &str) -> Resolution<AgentId> {
        Resolution::Unknown
    }
}

impl IdResolver for crate::state::AppState {
    fn resolve_job(&self, text: &str) -> Resolution<JobId> {
        resolve_short(
            text,
            self.jobs().values().map(|job| (job.id(), job.handle())),
        )
    }

    fn resolve_agent(&self, text: &str) -> Resolution<AgentId> {
        resolve_short(text, self.agents().keys().map(|id| (*id, None)))
    }
}

/// One matching rule for every id type: exact handle, else a suffix of the
/// canonical UUID text of at least [`MIN_SHORT_ID_CHARS`].
fn resolve_short<'a, T: Copy + Display>(
    text: &str,
    known: impl Iterator<Item = (T, Option<&'a str>)>,
) -> Resolution<T> {
    let wanted = text.trim().to_ascii_lowercase();
    if wanted.len() < MIN_SHORT_ID_CHARS {
        return Resolution::Unknown;
    }
    let mut found = None;
    let mut matched = 0usize;
    for (id, handle) in known {
        let by_handle = handle.is_some_and(|handle| handle.eq_ignore_ascii_case(&wanted));
        if by_handle || id.to_string().ends_with(&wanted) {
            matched += 1;
            found = Some(id);
        }
    }
    match (matched, found) {
        (1, Some(id)) => Resolution::Found(id),
        (0, _) => Resolution::Unknown,
        (n, _) => Resolution::Ambiguous { matched: n },
    }
}

/// An id type the parser can resolve from a short form. Types no panel
/// displays short (knowledge, sessions) resolve nothing and stay full-UUID.
trait ShortId: FromStr<Err = IdParseError> + Sized {
    fn resolve(resolver: &dyn IdResolver, text: &str) -> Resolution<Self>;
}

impl ShortId for JobId {
    fn resolve(resolver: &dyn IdResolver, text: &str) -> Resolution<Self> {
        resolver.resolve_job(text)
    }
}

impl ShortId for AgentId {
    fn resolve(resolver: &dyn IdResolver, text: &str) -> Resolution<Self> {
        resolver.resolve_agent(text)
    }
}

impl ShortId for KnowledgeId {
    fn resolve(_: &dyn IdResolver, _: &str) -> Resolution<Self> {
        Resolution::Unknown
    }
}

impl ShortId for SessionId {
    fn resolve(_: &dyn IdResolver, _: &str) -> Resolution<Self> {
        Resolution::Unknown
    }
}

/// Parse with no session to resolve short ids against: full UUIDs only.
pub fn parse_command(input: &str) -> Result<UiCommand, CommandError> {
    parse_command_in(input, &NoResolver)
}

/// Parse a slash command, resolving the short identifiers the session's
/// panels display through `resolver`.
pub fn parse_command_in(input: &str, resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    if input.len() > MAX_COMMAND_BYTES {
        return Err(CommandError::TooLong);
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CommandError::Empty);
    }
    if !trimmed.starts_with('/') {
        return Err(CommandError::NotACommand);
    }
    let cleaned = sanitize_untrusted(trimmed);
    let line = first_command_line(cleaned.as_ref())?;
    let body = line.trim().trim_start_matches('/');
    if body.is_empty() {
        return Ok(UiCommand::Help { topic: None });
    }
    let mut parts = body.split_whitespace();
    let Some(raw_name) = parts.next() else {
        return Ok(UiCommand::Help { topic: None });
    };
    let name = raw_name.to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();
    match resolve_name(&name) {
        Some("help") => parse_help(&args),
        Some("quit") => expect_none("quit", &args, UiCommand::Quit),
        Some("goal") => parse_goal(&args),
        Some("agents") => parse_agents(&args, resolver),
        Some("diff") => parse_diff(&args, resolver),
        Some("apply") => parse_apply(&args, resolver),
        Some("rollback") => parse_rollback(&args),
        Some("context") => parse_context(&args),
        Some("memory") => expect_none("memory", &args, UiCommand::OpenMemory),
        Some("knowledge") => parse_knowledge(&args, resolver),
        Some("playbook") => parse_playbook(&args),
        Some("trace") => parse_trace(&args),
        Some("insights") => parse_insights(&args),
        Some("handoff") => parse_handoff(&args),
        Some("takeover") => parse_takeover(&args),
        Some("control-return") => expect_none("control-return", &args, UiCommand::ControlReturn),
        Some("computer") => parse_computer(&args),
        Some("jobs") => parse_jobs(&args, resolver),
        Some("mcp") => parse_mcp(&args),
        Some("permissions") => parse_permissions(&args),
        Some("plugins") => parse_plugins(&args),
        Some("policy") => parse_policy(&args),
        Some("sandbox") => parse_sandbox(&args),
        Some("model") => parse_model(&args),
        Some("resume") => parse_resume(&args, resolver),
        Some("fork") => expect_none("fork", &args, UiCommand::Fork),
        Some("rewind") => parse_rewind(&args),
        Some("compact") => expect_none("compact", &args, UiCommand::Compact),
        Some(_) | None => Err(CommandError::UnknownCommand),
    }
}

/// Map a parsed command to a local or kernel-bound frontend action.
///
/// Approval-gated commands become [`KernelAction`]s that
/// [`KernelAction::requires_approval`] marks for [`kernel::KernelClient::approve`]
/// (or a later kernel method that still enters the broker). There is no local
/// apply/rollback/run path.
pub fn dispatch(command: UiCommand) -> FrontendAction {
    match command {
        UiCommand::Help { topic } => FrontendAction::InlineHelp(help_for(topic.as_deref())),
        UiCommand::Quit => FrontendAction::Quit,
        UiCommand::GoalShow => FrontendAction::Local(LocalAction::Open(Inspector::Goal)),
        UiCommand::GoalStart { statement } => {
            FrontendAction::Kernel(KernelAction::StartGoal { statement })
        }
        UiCommand::GoalPause => FrontendAction::Kernel(KernelAction::PauseGoal),
        UiCommand::GoalResume => FrontendAction::Kernel(KernelAction::ResumeGoal),
        UiCommand::GoalCancel => FrontendAction::Kernel(KernelAction::CancelGoal),
        UiCommand::GoalRun => FrontendAction::Kernel(KernelAction::RunGoal),
        UiCommand::GoalStop => FrontendAction::Kernel(KernelAction::StopGoal),
        UiCommand::GoalBudget {
            max_turns,
            max_tokens,
        } => FrontendAction::Kernel(KernelAction::ShowGoalBudget {
            max_turns,
            max_tokens,
        }),
        UiCommand::AgentsList => {
            FrontendAction::Local(LocalAction::Open(Inspector::Agents { id: None }))
        }
        UiCommand::AgentsShow { id } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Agents { id }))
        }
        UiCommand::AgentsPause { id } => FrontendAction::Kernel(KernelAction::PauseAgent { id }),
        UiCommand::AgentsResume { id } => FrontendAction::Kernel(KernelAction::ResumeAgent { id }),
        UiCommand::AgentsSleep { id } => FrontendAction::Kernel(KernelAction::SleepAgent { id }),
        UiCommand::AgentsCancel { id } => FrontendAction::Kernel(KernelAction::CancelAgent { id }),
        UiCommand::AgentsTerminate { id } => {
            FrontendAction::Kernel(KernelAction::TerminateAgent { id })
        }
        UiCommand::OpenDiff { agent } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Diff { agent }))
        }
        UiCommand::OpenMemory => FrontendAction::Local(LocalAction::Open(Inspector::Memory)),
        UiCommand::OpenPermissions => {
            FrontendAction::Local(LocalAction::Open(Inspector::Permissions))
        }
        UiCommand::ContextStatus | UiCommand::ContextInspect => {
            FrontendAction::Local(LocalAction::Open(Inspector::Context { query: None }))
        }
        UiCommand::ContextSearch { query } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Context { query: Some(query) }))
        }
        UiCommand::ContextReindex => FrontendAction::Kernel(KernelAction::ReindexContext),
        UiCommand::KnowledgeList | UiCommand::KnowledgeShow { .. } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Knowledge))
        }
        UiCommand::KnowledgeSuggest { text } => {
            FrontendAction::Kernel(KernelAction::SuggestKnowledge { text })
        }
        UiCommand::KnowledgeApprove { id } => {
            FrontendAction::Kernel(KernelAction::ApproveKnowledge { id })
        }
        UiCommand::KnowledgeReject { id } => {
            FrontendAction::Kernel(KernelAction::RejectKnowledge { id })
        }
        UiCommand::KnowledgeEdit { id } => {
            FrontendAction::Kernel(KernelAction::EditKnowledge { id })
        }
        UiCommand::PlaybookList | UiCommand::PlaybookShow { .. } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Playbook))
        }
        UiCommand::PlaybookRun { name } => {
            FrontendAction::Kernel(KernelAction::RunPlaybook { name })
        }
        UiCommand::PlaybookValidate { name } => {
            FrontendAction::Kernel(KernelAction::ValidatePlaybook { name })
        }
        UiCommand::TraceShow | UiCommand::TraceExport => {
            FrontendAction::Local(LocalAction::Open(Inspector::Trace))
        }
        UiCommand::InsightsShow | UiCommand::InsightsAnalyze | UiCommand::InsightsProposals => {
            FrontendAction::Local(LocalAction::Open(Inspector::Insights))
        }
        UiCommand::JobsList => FrontendAction::Local(LocalAction::Open(Inspector::Jobs {
            id: None,
            logs: false,
        })),
        UiCommand::JobsShow { id } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Jobs { id, logs: false }))
        }
        UiCommand::JobsLogs { id } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Jobs { id, logs: true }))
        }
        UiCommand::JobsCancel { id } => FrontendAction::Kernel(KernelAction::CancelJob { id }),
        UiCommand::ModelList | UiCommand::ModelDoctor => {
            FrontendAction::Local(LocalAction::Open(Inspector::Models))
        }
        UiCommand::ModelSelect { name } => {
            FrontendAction::Kernel(KernelAction::SelectModel { name })
        }
        UiCommand::McpList | UiCommand::McpDoctor => {
            FrontendAction::Local(LocalAction::Open(Inspector::Mcp))
        }
        UiCommand::McpAdd { target } => FrontendAction::Kernel(KernelAction::AddMcp { target }),
        UiCommand::McpRemove { name } => FrontendAction::Kernel(KernelAction::RemoveMcp { name }),
        UiCommand::McpAuth { name } => FrontendAction::Kernel(KernelAction::AuthMcp { name }),
        UiCommand::PermissionsAllow { pattern } => {
            FrontendAction::Local(LocalAction::Permissions(PermissionsIntent::Allow {
                pattern,
            }))
        }
        UiCommand::PermissionsRevoke { pattern } => {
            FrontendAction::Local(LocalAction::Permissions(PermissionsIntent::Revoke {
                pattern,
            }))
        }
        UiCommand::PluginList => FrontendAction::Local(LocalAction::Open(Inspector::Plugins)),
        UiCommand::PluginInstall { spec } => {
            FrontendAction::Kernel(KernelAction::InstallPlugin { spec })
        }
        UiCommand::PluginRemove { name } => {
            FrontendAction::Kernel(KernelAction::RemovePlugin { name })
        }
        UiCommand::PluginPermissions { name } => {
            FrontendAction::Kernel(KernelAction::SetPluginPermissions { name })
        }
        UiCommand::PolicyExplain | UiCommand::PolicyCheck => {
            FrontendAction::Local(LocalAction::Open(Inspector::Policy))
        }
        UiCommand::SandboxDoctor => FrontendAction::Local(LocalAction::Open(Inspector::Sandbox)),
        UiCommand::Resume { session } => {
            FrontendAction::Kernel(KernelAction::ResumeSession { session })
        }
        UiCommand::Fork => FrontendAction::Kernel(KernelAction::ForkSession),
        UiCommand::Rewind { to_seq } => {
            FrontendAction::Kernel(KernelAction::RewindSession { to_seq })
        }
        UiCommand::Compact => FrontendAction::Kernel(KernelAction::CompactSession),
        UiCommand::Apply { agent } => {
            FrontendAction::Kernel(KernelAction::ApplyChangeSet { agent })
        }
        UiCommand::Rollback { checkpoint } => {
            FrontendAction::Kernel(KernelAction::Rollback { checkpoint })
        }
        UiCommand::Handoff { dest, target } => {
            FrontendAction::Kernel(KernelAction::Handoff { dest, target })
        }
        UiCommand::Takeover { surface } => {
            FrontendAction::Kernel(KernelAction::Takeover { surface })
        }
        UiCommand::ControlReturn => FrontendAction::Kernel(KernelAction::ControlReturn),
        UiCommand::ComputerStatus => FrontendAction::Local(LocalAction::Open(Inspector::Computer)),
        UiCommand::ComputerObserve => FrontendAction::Kernel(KernelAction::ComputerObserve),
        UiCommand::ComputerRecord => FrontendAction::Kernel(KernelAction::ComputerRecord),
        UiCommand::ComputerTest => FrontendAction::Kernel(KernelAction::ComputerTest),
    }
}

/// Prefix-filter the slash catalog. `limit` is clamped to [`MAX_PALETTE_RESULTS`].
pub fn suggest(query: &str, limit: usize) -> Vec<PaletteEntry> {
    let limit = limit.min(MAX_PALETTE_RESULTS);
    if limit == 0 {
        return Vec::new();
    }
    let needle = normalize_query(query);
    let mut out = Vec::new();
    for spec in CATALOG {
        if out.len() >= limit {
            break;
        }
        if spec_matches(spec, &needle) {
            out.push(PaletteEntry {
                name: spec.name,
                usage: spec.usage,
                summary: spec.summary,
            });
        }
    }
    out
}

impl CommandError {
    /// Inline usage for the failed command, or the catalog for unknown names.
    pub fn help(&self) -> &'static str {
        match self {
            Self::Empty | Self::TooLong | Self::NotACommand | Self::UnknownCommand => {
                catalog_help()
            }
            Self::InvalidArgs { command }
            | Self::InvalidId { field: command }
            | Self::AmbiguousId { field: command, .. } => {
                usage_for(command).unwrap_or_else(catalog_help)
            }
        }
    }
}

impl KernelAction {
    /// Policy-gated work must go through the kernel approval/broker path.
    pub fn requires_approval(&self) -> bool {
        matches!(
            self,
            Self::ApplyChangeSet { .. }
                | Self::Rollback { .. }
                | Self::RunPlaybook { .. }
                | Self::ApproveKnowledge { .. }
                | Self::RejectKnowledge { .. }
                | Self::EditKnowledge { .. }
                | Self::AddMcp { .. }
                | Self::RemoveMcp { .. }
                | Self::AuthMcp { .. }
                | Self::InstallPlugin { .. }
                | Self::RemovePlugin { .. }
                | Self::SetPluginPermissions { .. }
                | Self::Handoff { .. }
                | Self::Takeover { .. }
                | Self::ComputerObserve
                | Self::ComputerRecord
                | Self::ComputerTest
        )
    }

    /// Kernel client surface that must receive this action.
    pub fn kernel_api(&self) -> KernelApi {
        match self {
            Self::ApplyChangeSet { .. }
            | Self::Rollback { .. }
            | Self::RunPlaybook { .. }
            | Self::ApproveKnowledge { .. }
            | Self::RejectKnowledge { .. }
            | Self::EditKnowledge { .. }
            | Self::AddMcp { .. }
            | Self::RemoveMcp { .. }
            | Self::AuthMcp { .. }
            | Self::InstallPlugin { .. }
            | Self::RemovePlugin { .. }
            | Self::SetPluginPermissions { .. }
            | Self::Handoff { .. }
            | Self::Takeover { .. }
            | Self::ComputerObserve
            | Self::ComputerRecord
            | Self::ComputerTest => KernelApi::Approve,
            Self::CancelAgent { .. } | Self::TerminateAgent { .. } | Self::CancelJob { .. } => {
                KernelApi::Interrupt
            }
            Self::ForkSession => KernelApi::ForkSession,
            Self::RewindSession { .. } => KernelApi::Rewind,
            Self::StartGoal { .. } => KernelApi::SubmitTurn,
            _ => KernelApi::Dispatch,
        }
    }
}

impl Inspector {
    /// Existing reducer route, when the inspector already has one.
    pub fn route(&self) -> Option<UiRoute> {
        match self {
            Self::Agents { .. } => Some(UiRoute::Agents),
            Self::Diff { .. } => Some(UiRoute::Diff),
            Self::Context { .. } => Some(UiRoute::Context),
            Self::Memory => Some(UiRoute::Memory),
            Self::Jobs { .. } => Some(UiRoute::Jobs),
            Self::Goal => Some(UiRoute::Goals),
            Self::Knowledge
            | Self::Playbook
            | Self::Trace
            | Self::Insights
            | Self::Models
            | Self::Mcp
            | Self::Plugins
            | Self::Policy
            | Self::Sandbox
            | Self::Permissions
            | Self::Computer => None,
        }
    }
}

impl InlineHelp {
    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }

    pub fn usage(&self) -> &str {
        &self.usage
    }
}

impl PaletteEntry {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn usage(&self) -> &'static str {
        self.usage
    }

    pub fn summary(&self) -> &'static str {
        self.summary
    }
}

impl Display for CommandError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty command"),
            Self::TooLong => f.write_str("command exceeds bound"),
            Self::NotACommand => f.write_str("not a slash command"),
            Self::UnknownCommand => f.write_str("unknown command"),
            Self::InvalidArgs { .. } => f.write_str("invalid arguments"),
            Self::InvalidId { .. } => f.write_str("invalid identifier"),
            Self::AmbiguousId { matched, .. } => {
                write!(f, "ambiguous identifier: matches {matched} in this session")
            }
        }
    }
}

impl Error for CommandError {}

impl Display for HandoffDest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Daemon => "daemon",
            Self::Remote => "remote",
        })
    }
}

impl Display for TakeoverSurface {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Terminal => "terminal",
            Self::Browser => "browser",
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        })
    }
}

fn parse_help(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] => Ok(UiCommand::Help { topic: None }),
        [topic] => {
            let topic = topic.to_ascii_lowercase();
            if resolve_name(&topic).is_none() {
                return Err(CommandError::UnknownCommand);
            }
            Ok(UiCommand::Help { topic: Some(topic) })
        }
        _ => Err(invalid("help")),
    }
}

fn parse_goal(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["show"] => Ok(UiCommand::GoalShow),
        ["pause"] => Ok(UiCommand::GoalPause),
        ["resume"] => Ok(UiCommand::GoalResume),
        ["cancel"] => Ok(UiCommand::GoalCancel),
        ["start"] => Err(invalid("goal")),
        ["start", rest @ ..] => Ok(UiCommand::GoalStart {
            statement: join_text("goal", rest)?,
        }),
        ["budget", rest @ ..] => parse_goal_budget(rest),
        ["run"] => Ok(UiCommand::GoalRun),
        ["stop"] => Ok(UiCommand::GoalStop),
        _ => Err(invalid("goal")),
    }
}

fn parse_goal_budget(args: &[&str]) -> Result<UiCommand, CommandError> {
    let mut max_turns = None;
    let mut max_tokens = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "turns" => {
                let value = args.get(i + 1).ok_or_else(|| invalid("goal"))?;
                max_turns = Some(parse_u64("goal", value)?);
                i += 2;
            }
            "tokens" => {
                let value = args.get(i + 1).ok_or_else(|| invalid("goal"))?;
                max_tokens = Some(parse_u64("goal", value)?);
                i += 2;
            }
            _ => return Err(invalid("goal")),
        }
    }
    Ok(UiCommand::GoalBudget {
        max_turns,
        max_tokens,
    })
}

fn parse_agents(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::AgentsList),
        ["show", rest @ ..] => Ok(UiCommand::AgentsShow {
            id: optional_id("agents", rest, resolver)?,
        }),
        ["pause", rest @ ..] => Ok(UiCommand::AgentsPause {
            id: optional_id("agents", rest, resolver)?,
        }),
        ["resume", rest @ ..] => Ok(UiCommand::AgentsResume {
            id: optional_id("agents", rest, resolver)?,
        }),
        ["sleep", rest @ ..] => Ok(UiCommand::AgentsSleep {
            id: optional_id("agents", rest, resolver)?,
        }),
        ["cancel", rest @ ..] => Ok(UiCommand::AgentsCancel {
            id: optional_id("agents", rest, resolver)?,
        }),
        ["terminate", rest @ ..] => Ok(UiCommand::AgentsTerminate {
            id: optional_id("agents", rest, resolver)?,
        }),
        _ => Err(invalid("agents")),
    }
}

fn parse_diff(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::OpenDiff {
        agent: optional_agent_flag("diff", args, resolver)?,
    })
}

fn parse_apply(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::Apply {
        agent: optional_agent_flag("apply", args, resolver)?,
    })
}

fn parse_rollback(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] => Ok(UiCommand::Rollback { checkpoint: None }),
        [raw] => Ok(UiCommand::Rollback {
            checkpoint: Some(parse_ident("rollback", raw)?),
        }),
        _ => Err(invalid("rollback")),
    }
}

fn parse_context(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["status"] => Ok(UiCommand::ContextStatus),
        ["inspect"] => Ok(UiCommand::ContextInspect),
        ["reindex"] => Ok(UiCommand::ContextReindex),
        ["search"] => Err(invalid("context")),
        ["search", rest @ ..] => Ok(UiCommand::ContextSearch {
            query: join_text("context", rest)?,
        }),
        _ => Err(invalid("context")),
    }
}

fn parse_knowledge(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::KnowledgeList),
        ["show", rest @ ..] => Ok(UiCommand::KnowledgeShow {
            id: optional_id("knowledge", rest, resolver)?,
        }),
        ["suggest"] => Err(invalid("knowledge")),
        ["suggest", rest @ ..] => Ok(UiCommand::KnowledgeSuggest {
            text: join_text("knowledge", rest)?,
        }),
        ["approve", rest @ ..] => Ok(UiCommand::KnowledgeApprove {
            id: require_id("knowledge", rest, resolver)?,
        }),
        ["reject", rest @ ..] => Ok(UiCommand::KnowledgeReject {
            id: require_id("knowledge", rest, resolver)?,
        }),
        ["edit", rest @ ..] => Ok(UiCommand::KnowledgeEdit {
            id: require_id("knowledge", rest, resolver)?,
        }),
        _ => Err(invalid("knowledge")),
    }
}

fn parse_playbook(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::PlaybookList),
        ["show", rest @ ..] => Ok(UiCommand::PlaybookShow {
            name: require_ident("playbook", rest)?,
        }),
        ["run", rest @ ..] => Ok(UiCommand::PlaybookRun {
            name: require_ident("playbook", rest)?,
        }),
        ["validate", rest @ ..] => Ok(UiCommand::PlaybookValidate {
            name: require_ident("playbook", rest)?,
        }),
        _ => Err(invalid("playbook")),
    }
}

fn parse_trace(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["show"] => Ok(UiCommand::TraceShow),
        ["export"] => Ok(UiCommand::TraceExport),
        _ => Err(invalid("trace")),
    }
}

fn parse_insights(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["show"] => Ok(UiCommand::InsightsShow),
        ["analyze"] => Ok(UiCommand::InsightsAnalyze),
        ["proposals"] => Ok(UiCommand::InsightsProposals),
        _ => Err(invalid("insights")),
    }
}

fn parse_handoff(args: &[&str]) -> Result<UiCommand, CommandError> {
    let dest = match args.first().copied() {
        Some("local") => HandoffDest::Local,
        Some("daemon") => HandoffDest::Daemon,
        Some("remote") => HandoffDest::Remote,
        _ => return Err(invalid("handoff")),
    };
    let target = match (dest, &args[1..]) {
        (_, []) => None,
        (HandoffDest::Remote, [raw]) => Some(parse_ident("handoff", raw)?),
        (HandoffDest::Local | HandoffDest::Daemon, [_]) => return Err(invalid("handoff")),
        _ => return Err(invalid("handoff")),
    };
    if dest == HandoffDest::Remote && target.is_none() {
        return Err(invalid("handoff"));
    }
    Ok(UiCommand::Handoff { dest, target })
}

fn parse_takeover(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        ["terminal"] => Ok(UiCommand::Takeover {
            surface: TakeoverSurface::Terminal,
        }),
        ["browser"] => Ok(UiCommand::Takeover {
            surface: TakeoverSurface::Browser,
        }),
        ["desktop"] => Ok(UiCommand::Takeover {
            surface: TakeoverSurface::Desktop,
        }),
        ["mobile"] => Ok(UiCommand::Takeover {
            surface: TakeoverSurface::Mobile,
        }),
        _ => Err(invalid("takeover")),
    }
}

fn parse_computer(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["status"] => Ok(UiCommand::ComputerStatus),
        ["observe"] => Ok(UiCommand::ComputerObserve),
        ["record"] => Ok(UiCommand::ComputerRecord),
        ["test"] => Ok(UiCommand::ComputerTest),
        _ => Err(invalid("computer")),
    }
}

fn parse_jobs(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::JobsList),
        ["show", rest @ ..] => Ok(UiCommand::JobsShow {
            id: optional_id("jobs", rest, resolver)?,
        }),
        ["cancel", rest @ ..] => Ok(UiCommand::JobsCancel {
            id: optional_id("jobs", rest, resolver)?,
        }),
        ["logs", rest @ ..] => Ok(UiCommand::JobsLogs {
            id: optional_id("jobs", rest, resolver)?,
        }),
        _ => Err(invalid("jobs")),
    }
}

fn parse_mcp(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::McpList),
        ["doctor"] => Ok(UiCommand::McpDoctor),
        ["add", rest @ ..] => Ok(UiCommand::McpAdd {
            target: require_ident("mcp", rest)?,
        }),
        ["remove", rest @ ..] => Ok(UiCommand::McpRemove {
            name: require_ident("mcp", rest)?,
        }),
        ["auth", rest @ ..] => Ok(UiCommand::McpAuth {
            name: require_ident("mcp", rest)?,
        }),
        _ => Err(invalid("mcp")),
    }
}

/// `/permissions [list|allow <pattern>|revoke <pattern>]`.
///
/// Bare `/permissions` and `list` both open the inspector, which renders the
/// project's real grants; `allow`/`revoke` write them.
fn parse_permissions(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::OpenPermissions),
        ["allow", rest @ ..] => Ok(UiCommand::PermissionsAllow {
            pattern: require_pattern("permissions", rest)?,
        }),
        ["revoke", rest @ ..] => Ok(UiCommand::PermissionsRevoke {
            pattern: require_pattern("permissions", rest)?,
        }),
        _ => Err(invalid("permissions")),
    }
}

/// A tool-permission pattern operand: `Tool` or `Tool(arg-glob)`.
///
/// Joined rather than requiring one shell word, because the glob half is
/// routinely written with spaces (`shell_exec(git *)`) and a composer line is
/// split on whitespace. Validated only for shape here — the authority on the
/// grammar is `permissions::ToolPattern::parse`, which the host runs before
/// anything is written, so this never becomes a second parser.
fn require_pattern(command: &'static str, args: &[&str]) -> Result<String, CommandError> {
    if args.is_empty() {
        return Err(invalid(command));
    }
    let joined = args.join(" ");
    if joined.len() > MAX_COMMAND_BYTES {
        return Err(CommandError::TooLong);
    }
    Ok(joined)
}

fn parse_plugins(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::PluginList),
        ["install", rest @ ..] => Ok(UiCommand::PluginInstall {
            spec: require_ident("plugins", rest)?,
        }),
        ["remove", rest @ ..] => Ok(UiCommand::PluginRemove {
            name: require_ident("plugins", rest)?,
        }),
        ["permissions", rest @ ..] => Ok(UiCommand::PluginPermissions {
            name: require_ident("plugins", rest)?,
        }),
        _ => Err(invalid("plugins")),
    }
}

fn parse_policy(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["explain"] => Ok(UiCommand::PolicyExplain),
        ["check"] => Ok(UiCommand::PolicyCheck),
        _ => Err(invalid("policy")),
    }
}

fn parse_sandbox(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["doctor"] => Ok(UiCommand::SandboxDoctor),
        _ => Err(invalid("sandbox")),
    }
}

fn parse_model(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::ModelList),
        ["doctor"] => Ok(UiCommand::ModelDoctor),
        ["select", rest @ ..] => Ok(UiCommand::ModelSelect {
            name: require_ident("model", rest)?,
        }),
        _ => Err(invalid("model")),
    }
}

fn parse_resume(args: &[&str], resolver: &dyn IdResolver) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::Resume {
        session: optional_id("resume", args, resolver)?,
    })
}

/// `/rewind <seq>`. The sequence is required: the host's rewind path early-
/// returns on `None`, so a bare `/rewind` parsed successfully and then did
/// nothing at all, silently — the usage even advertised it as optional.
fn parse_rewind(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [raw] => Ok(UiCommand::Rewind {
            to_seq: Some(parse_u64("rewind", raw)?),
        }),
        _ => Err(invalid("rewind")),
    }
}

fn expect_none(
    command: &'static str,
    args: &[&str],
    value: UiCommand,
) -> Result<UiCommand, CommandError> {
    if args.is_empty() {
        Ok(value)
    } else {
        Err(invalid(command))
    }
}

fn first_command_line(input: &str) -> Result<&str, CommandError> {
    let mut lines = input.split('\n');
    let first = lines.next().unwrap_or("").trim();
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(invalid("slash"));
    }
    Ok(first)
}

fn resolve_name(name: &str) -> Option<&'static str> {
    for spec in CATALOG {
        if spec.name == name || spec.aliases.contains(&name) {
            return Some(spec.name);
        }
    }
    None
}

fn usage_for(name: &str) -> Option<&'static str> {
    if name == "slash" {
        return Some(catalog_help());
    }
    CATALOG
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .map(|spec| spec.usage)
}

fn help_for(topic: Option<&str>) -> InlineHelp {
    let usage = topic
        .and_then(usage_for)
        .unwrap_or_else(catalog_help)
        .to_owned();
    InlineHelp {
        topic: topic.map(str::to_owned),
        usage,
    }
}

fn optional_agent_flag(
    command: &'static str,
    args: &[&str],
    resolver: &dyn IdResolver,
) -> Result<Option<AgentId>, CommandError> {
    match args {
        [] => Ok(None),
        ["--agent", raw] => parse_typed_id(command, raw, resolver).map(Some),
        _ => Err(invalid(command)),
    }
}

fn optional_id<T: ShortId>(
    command: &'static str,
    args: &[&str],
    resolver: &dyn IdResolver,
) -> Result<Option<T>, CommandError> {
    match args {
        [] => Ok(None),
        [raw] => parse_typed_id(command, raw, resolver).map(Some),
        _ => Err(invalid(command)),
    }
}

fn require_id<T: ShortId>(
    command: &'static str,
    args: &[&str],
    resolver: &dyn IdResolver,
) -> Result<T, CommandError> {
    match optional_id(command, args, resolver)? {
        Some(id) => Ok(id),
        None => Err(invalid(command)),
    }
}

/// A full UUID always parses; otherwise the text is resolved as the short
/// form the panels display. See [`IdResolver`] for the rule.
fn parse_typed_id<T: ShortId>(
    field: &'static str,
    raw: &str,
    resolver: &dyn IdResolver,
) -> Result<T, CommandError> {
    if let Ok(id) = T::from_str(raw) {
        return Ok(id);
    }
    match T::resolve(resolver, raw) {
        Resolution::Found(id) => Ok(id),
        Resolution::Ambiguous { matched } => Err(CommandError::AmbiguousId { field, matched }),
        Resolution::Unknown => Err(CommandError::InvalidId { field }),
    }
}

fn require_ident(command: &'static str, args: &[&str]) -> Result<String, CommandError> {
    match args {
        [raw] => parse_ident(command, raw),
        _ => Err(invalid(command)),
    }
}

fn parse_ident(command: &'static str, raw: &str) -> Result<String, CommandError> {
    let cleaned = sanitize_untrusted(raw);
    if cleaned.len() > MAX_ARG_BYTES || cleaned.is_empty() {
        return Err(invalid(command));
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
    {
        return Err(invalid(command));
    }
    if cleaned.contains("://") {
        return Err(invalid(command));
    }
    Ok(cleaned.into_owned())
}

fn join_text(command: &'static str, parts: &[&str]) -> Result<String, CommandError> {
    if parts.is_empty() {
        return Err(invalid(command));
    }
    let joined = parts.join(" ");
    let cleaned = sanitize_untrusted(&joined);
    let text = cleaned.trim();
    if text.is_empty() || text.len() > MAX_ARG_BYTES {
        return Err(invalid(command));
    }
    Ok(text.to_owned())
}

fn parse_u64(command: &'static str, raw: &str) -> Result<u64, CommandError> {
    if raw.len() > 20 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(command));
    }
    raw.parse().map_err(|_| invalid(command))
}

fn invalid(command: &'static str) -> CommandError {
    CommandError::InvalidArgs { command }
}

fn normalize_query(query: &str) -> String {
    let cleaned = sanitize_untrusted(query.trim());
    let body = cleaned.trim().trim_start_matches('/');
    body.to_ascii_lowercase()
}

fn spec_matches(spec: &CommandSpec, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if spec.name.starts_with(needle) {
        return true;
    }
    if spec.aliases.iter().any(|alias| alias.starts_with(needle)) {
        return true;
    }
    let usage = spec.usage.trim_start_matches('/');
    usage.starts_with(needle)
        || usage
            .split_whitespace()
            .any(|word| word.starts_with(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "01234567-89ab-7cde-89ab-0123456789ab";

    fn parse_ok(input: &str) -> UiCommand {
        parse_command(input).expect("command")
    }

    fn parse_err(input: &str) -> CommandError {
        parse_command(input).expect_err("expected help")
    }

    #[test]
    fn parse_goal_pause_is_typed_command() {
        assert_eq!(parse_ok("/goal pause"), UiCommand::GoalPause);
        assert_eq!(
            dispatch(UiCommand::GoalPause),
            FrontendAction::Kernel(KernelAction::PauseGoal)
        );
    }

    #[test]
    fn parse_goal_run_and_stop_are_typed_commands() {
        assert_eq!(parse_ok("/goal run"), UiCommand::GoalRun);
        assert_eq!(
            dispatch(UiCommand::GoalRun),
            FrontendAction::Kernel(KernelAction::RunGoal)
        );
        assert_eq!(parse_ok("/goal stop"), UiCommand::GoalStop);
        assert_eq!(
            dispatch(UiCommand::GoalStop),
            FrontendAction::Kernel(KernelAction::StopGoal)
        );
        // Neither takes arguments.
        assert_eq!(
            parse_err("/goal run now"),
            CommandError::InvalidArgs { command: "goal" }
        );
        assert_eq!(
            parse_err("/goal stop please"),
            CommandError::InvalidArgs { command: "goal" }
        );
    }

    #[test]
    fn unknown_and_invalid_args_produce_inline_help() {
        let unknown = parse_err("/nope");
        assert_eq!(unknown, CommandError::UnknownCommand);
        assert!(unknown.help().contains("/goal"));
        assert_eq!(unknown.to_string(), "unknown command");

        let invalid = parse_err("/goal foo");
        assert_eq!(invalid, CommandError::InvalidArgs { command: "goal" });
        assert_eq!(
            invalid.help(),
            "/goal [show|start <text>|pause|resume|cancel|budget [turns N] [tokens N]|run|stop]"
        );
        assert!(!invalid.to_string().contains("foo"));

        let missing = parse_err("/goal start");
        assert_eq!(missing.help(), invalid.help());

        let bad_id = parse_err("/agents pause not-a-uuid");
        assert_eq!(bad_id, CommandError::InvalidId { field: "agents" });
        assert!(bad_id.help().starts_with("/agents"));
        assert!(!bad_id.to_string().contains("not-a-uuid"));
    }

    #[test]
    fn approval_commands_route_to_kernel_api() {
        let apply = parse_ok("/apply");
        assert_eq!(apply, UiCommand::Apply { agent: None });
        let action = dispatch(apply);
        match action {
            FrontendAction::Kernel(kernel) => {
                assert!(kernel.requires_approval());
                assert_eq!(kernel.kernel_api(), KernelApi::Approve);
            }
            other => panic!("apply must be kernel-bound, got {other:?}"),
        }

        let playbook = parse_ok("/playbook run ship-it");
        match dispatch(playbook) {
            FrontendAction::Kernel(kernel) => {
                assert!(kernel.requires_approval());
                assert_eq!(kernel.kernel_api(), KernelApi::Approve);
            }
            other => panic!("playbook run must be kernel-bound, got {other:?}"),
        }

        let takeover = parse_ok("/takeover browser");
        match dispatch(takeover) {
            FrontendAction::Kernel(kernel) => {
                assert!(kernel.requires_approval());
                assert_eq!(
                    kernel,
                    KernelAction::Takeover {
                        surface: TakeoverSurface::Browser
                    }
                );
            }
            other => panic!("takeover must be kernel-bound, got {other:?}"),
        }

        assert!(matches!(
            dispatch(parse_ok("/rollback ckpt_1")),
            FrontendAction::Kernel(KernelAction::Rollback { .. })
        ));
        assert!(!matches!(
            dispatch(parse_ok("/apply")),
            FrontendAction::Local(_)
        ));
    }

    /// Fold one `job.started` into `state`, as the producer records it.
    fn with_job(
        state: crate::state::AppState,
        seq: u64,
        id: &str,
        handle: Option<&str>,
    ) -> crate::state::AppState {
        use crate::state::{UiEvent, reduce};
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
        use protocol::{EventId, RedactionClass, TraceId};

        let session: SessionId = "019c0000-0000-7000-8000-000000000010"
            .parse()
            .expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        let kind = if seq == 1 {
            EventKind::SessionCreated
        } else {
            EventKind::JobStarted
        };
        let mut payload = serde_json::json!({"job_id": id});
        if seq == 1 {
            payload = serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"});
        } else if let Some(handle) = handle {
            payload["handle"] = serde_json::Value::String(handle.to_owned());
        }
        let event = EventEnvelope::new(
            format!("019c0000-0000-7000-8000-{seq:012x}")
                .parse::<EventId>()
                .expect("event id"),
            session,
            seq,
            "2026-09-11T09:00:00.000Z"
                .parse::<RecordedAt>()
                .expect("recorded_at"),
            actor,
            TraceId::new(),
            kind,
            RedactionClass::Project,
            payload,
        );
        reduce(state, &UiEvent::Kernel(event))
    }

    const JOB_A: &str = "019c0000-0000-7000-8000-0000000000aa";
    const JOB_B: &str = "019c0000-0000-7000-8000-0000000000ab";

    /// Two jobs that share every prefix and differ in the last character —
    /// what UUIDv7 gives two jobs started in one session — one with the
    /// handle the producer records.
    fn session_with_two_jobs() -> crate::state::AppState {
        let state = with_job(crate::state::AppState::new(), 1, "", None);
        let state = with_job(state, 2, JOB_A, Some("job-1"));
        with_job(state, 3, JOB_B, None)
    }

    #[test]
    fn short_ids_resolve_by_handle_and_by_tail() {
        // The rule: a panel must never show an identifier the commands
        // refuse. Panels show a job's `job-N` handle and, failing one, the
        // id's random tail — so both must parse.
        let state = session_with_two_jobs();
        let a: JobId = JOB_A.parse().expect("id");
        let b: JobId = JOB_B.parse().expect("id");

        assert_eq!(
            parse_command_in("/jobs show job-1", &state),
            Ok(UiCommand::JobsShow { id: Some(a) }),
            "the transcript's handle resolves"
        );
        assert_eq!(
            parse_command_in("/jobs show 0000000000ab", &state),
            Ok(UiCommand::JobsShow { id: Some(b) }),
            "the tail the panel shows resolves"
        );
        assert_eq!(
            parse_command_in("/jobs cancel 00ab", &state),
            Ok(UiCommand::JobsCancel { id: Some(b) }),
            "a shorter suffix resolves while it is unique"
        );
        assert_eq!(
            parse_command_in(&format!("/jobs show {a}"), &state),
            Ok(UiCommand::JobsShow { id: Some(a) }),
            "a full UUID always parses"
        );

        // Two UUIDv7s minted seconds apart share every leading character,
        // so a git-style prefix would match both — which is why a prefix
        // is not what is matched, and why this is refused rather than
        // resolved to the first.
        assert_eq!(
            parse_command_in("/jobs show 019c0000-0000-7000-8000-0000000000a", &state),
            Err(CommandError::InvalidId { field: "jobs" }),
        );
        // Below the minimum nothing resolves: an accidental `/jobs cancel
        // ab` must be a parse error, not a cancellation.
        assert_eq!(
            parse_command_in("/jobs cancel ab", &state),
            Err(CommandError::InvalidId { field: "jobs" }),
        );
        // With no session behind the parse, behavior is unchanged: full
        // UUIDs only.
        assert_eq!(
            parse_command("/jobs show job-1"),
            Err(CommandError::InvalidId { field: "jobs" }),
        );
    }

    #[test]
    fn an_ambiguous_short_id_is_refused_with_the_count() {
        // Two jobs whose tails share a suffix. The resolver must refuse
        // rather than pick either — the command asking can be `cancel`.
        let state = session_with_two_jobs();
        let state = with_job(state, 4, "019c0000-0000-7000-8000-0000000100ab", None);
        let e: JobId = "019c0000-0000-7000-8000-0000000100ab".parse().expect("id");

        // `…0000000000ab` and `…0000000100ab` both end in `00ab`.
        assert_eq!(
            parse_command_in("/jobs cancel 00ab", &state),
            Err(CommandError::AmbiguousId {
                field: "jobs",
                matched: 2
            }),
            "a shared suffix must be refused with the count, never picked from"
        );
        // More of the id disambiguates.
        assert_eq!(
            parse_command_in("/jobs cancel 100ab", &state),
            Ok(UiCommand::JobsCancel { id: Some(e) }),
        );
        // And the refusal names the command, so the usage line it carries
        // is the right one.
        let err = parse_command_in("/jobs cancel 00ab", &state).expect_err("ambiguous");
        assert!(err.help().contains("/jobs"), "{}", err.help());
        assert_eq!(
            err.to_string(),
            "ambiguous identifier: matches 2 in this session"
        );
    }

    #[test]
    fn inspectors_are_local_chrome() {
        assert_eq!(
            dispatch(parse_ok("/agents")),
            FrontendAction::Local(LocalAction::Open(Inspector::Agents { id: None }))
        );
        assert_eq!(
            Inspector::Agents { id: None }.route(),
            Some(UiRoute::Agents)
        );
        // The id `/agents show` parses has to survive dispatch: it used to
        // be discarded here, so the panel could not tell which agent was
        // asked about.
        let named = "019c0000-0000-7000-8000-0000000000a7";
        assert_eq!(
            dispatch(parse_ok(&format!("/agents show {named}"))),
            FrontendAction::Local(LocalAction::Open(Inspector::Agents {
                id: Some(named.parse().expect("agent id")),
            }))
        );
        assert_eq!(parse_ok("/models"), UiCommand::ModelList);
        assert_eq!(parse_ok("/memory"), UiCommand::OpenMemory);
        assert_eq!(parse_ok("/quit"), UiCommand::Quit);
        assert_eq!(dispatch(UiCommand::Quit), FrontendAction::Quit);
    }

    #[test]
    fn agent_flag_and_goal_start_are_typed() {
        let id = AgentId::from_str(AGENT).expect("agent");
        assert_eq!(
            parse_ok(&format!("/diff --agent {AGENT}")),
            UiCommand::OpenDiff { agent: Some(id) }
        );
        assert_eq!(
            parse_ok("/goal start fix the flaky test"),
            UiCommand::GoalStart {
                statement: "fix the flaky test".into(),
            }
        );
        assert_eq!(
            parse_ok("/goal budget turns 4 tokens 8000"),
            UiCommand::GoalBudget {
                max_turns: Some(4),
                max_tokens: Some(8000),
            }
        );
    }

    #[test]
    fn rejects_shell_strings_and_escapes() {
        assert_eq!(
            parse_err("/rollback rm -rf /"),
            CommandError::InvalidArgs {
                command: "rollback"
            }
        );
        assert_eq!(
            parse_err("/playbook run $(reboot)"),
            CommandError::InvalidArgs {
                command: "playbook"
            }
        );
        assert_eq!(
            parse_err("/apply --agent ../../etc/passwd"),
            CommandError::InvalidId { field: "apply" }
        );
        let escaped =
            parse_ok("/goal start pre\u{1b}]8;;https://evil.example\u{07}fix\u{1b}]8;;\u{07}");
        assert_eq!(
            escaped,
            UiCommand::GoalStart {
                statement: "prefix".into(),
            }
        );
        assert_eq!(parse_command("hello"), Err(CommandError::NotACommand));
        assert_eq!(parse_command(""), Err(CommandError::Empty));
        let long = format!("/{}", "a".repeat(MAX_COMMAND_BYTES));
        assert_eq!(parse_command(&long), Err(CommandError::TooLong));
    }

    #[test]
    fn help_and_palette_are_bounded() {
        assert_eq!(parse_ok("/"), UiCommand::Help { topic: None });
        match dispatch(parse_ok("/help goal")) {
            FrontendAction::InlineHelp(help) => {
                assert_eq!(help.topic(), Some("goal"));
                assert!(help.usage().starts_with("/goal"));
            }
            other => panic!("expected inline help, got {other:?}"),
        }
        let rows = suggest("/go", 32);
        assert!(rows.len() <= MAX_PALETTE_RESULTS);
        assert!(rows.iter().any(|row| row.name() == "goal"));
        assert!(suggest("/zzz", 8).is_empty());
        assert!(
            suggest("/goal", 8)
                .iter()
                .any(|row| row.usage().starts_with("/goal"))
        );
        assert_eq!(suggest("", 0).len(), 0);
        assert!(catalog_help().contains("/control-return"));
    }

    #[test]
    fn the_catalog_listing_is_the_catalog() {
        // Replaces a "golden" test that read `const GOLDEN_HELP: &str =
        // CATALOG_HELP;` and then asserted `GOLDEN_HELP == CATALOG_HELP` —
        // a tautology that could never fail, and did not, while the two
        // lists drifted for four commands.
        let listed: Vec<&str> = catalog_help().lines().collect();
        let expected: Vec<&str> = CATALOG.iter().map(|spec| spec.usage).collect();
        assert_eq!(listed, expected);

        // Bare `/help` and `/help <name>` must agree about a command's
        // arguments: that is exactly what had drifted. `/goal`,
        // `/knowledge`, `/mcp` and `/plugins` each documented fewer operands
        // in the catalog listing than in their own usage.
        for spec in CATALOG {
            let own = usage_for(spec.name).expect("every catalog command has its own usage");
            assert!(
                catalog_help().lines().any(|line| line == own),
                "`/help {}` shows {own:?}, which is not the line bare `/help` lists",
                spec.name
            );
        }
    }

    #[test]
    fn every_catalog_entry_is_parseable_and_uniquely_named() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in CATALOG {
            assert!(seen.insert(spec.name), "duplicate command: {}", spec.name);
            for alias in spec.aliases {
                assert!(seen.insert(alias), "duplicate alias: {alias}");
            }
            assert!(
                spec.usage.starts_with(&format!("/{}", spec.name)),
                "`{}`'s usage does not start with its own name: {:?}",
                spec.name,
                spec.usage
            );
            assert!(!spec.summary.is_empty(), "{} has no summary", spec.name);
        }
    }

    #[test]
    fn control_and_session_commands_parse() {
        assert_eq!(parse_ok("/control-return"), UiCommand::ControlReturn);
        assert_eq!(parse_ok("/fork"), UiCommand::Fork);
        assert_eq!(
            parse_ok("/rewind 12"),
            UiCommand::Rewind { to_seq: Some(12) }
        );
        assert_eq!(
            parse_ok("/handoff remote worker-a"),
            UiCommand::Handoff {
                dest: HandoffDest::Remote,
                target: Some("worker-a".into()),
            }
        );
        assert_eq!(
            parse_err("/handoff remote"),
            CommandError::InvalidArgs { command: "handoff" }
        );
        assert!(matches!(
            dispatch(parse_ok("/fork")),
            FrontendAction::Kernel(KernelAction::ForkSession)
        ));
        assert!(!KernelAction::ForkSession.requires_approval());
        assert_eq!(
            KernelAction::ForkSession.kernel_api(),
            KernelApi::ForkSession
        );
    }
}
