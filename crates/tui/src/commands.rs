//! Slash-command palette and typed frontend action router.
//!
//! Composer text that starts with `/` is parsed into [`UiCommand`]. Dispatch
//! maps those values to local chrome or [`KernelAction`]. The router never
//! treats input as a shell string and never applies policy-gated work locally.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

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
    InvalidArgs { command: &'static str },
    InvalidId { field: &'static str },
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
}

/// Inspector the TUI can focus. Mapping onto [`UiRoute`] is best-effort.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Inspector {
    Agents,
    Diff { agent: Option<AgentId> },
    Goal,
    Context,
    Memory,
    Knowledge,
    Playbook,
    Trace,
    Insights,
    Jobs,
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
        usage: "/knowledge [list|show|suggest <text>|approve|reject|edit] [id]",
        summary: "browse or govern knowledge candidates",
    },
    CommandSpec {
        name: "playbook",
        aliases: &[],
        usage: "/playbook [list|show|run|validate] [name]",
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
        usage: "/handoff local|daemon|remote [target]",
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
        usage: "/mcp [list|add <target>|remove|auth|doctor] [name]",
        summary: "inspect or request MCP changes",
    },
    CommandSpec {
        name: "permissions",
        aliases: &[],
        usage: "/permissions",
        summary: "open the permission inspector",
    },
    CommandSpec {
        name: "plugins",
        aliases: &[],
        usage: "/plugins [list|install <spec>|remove|permissions] [name]",
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
        summary: "fork the session through the kernel",
    },
    CommandSpec {
        name: "rewind",
        aliases: &[],
        usage: "/rewind [seq]",
        summary: "rewind the session through the kernel",
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        usage: "/compact",
        summary: "request kernel transcript compaction",
    },
];

const CATALOG_HELP: &str = "\
/help [command]
/quit
/model [list|select <name>|doctor]
/agents [list|show|pause|resume|sleep|cancel|terminate] [id]
/goal [show|start <text>|pause|resume|cancel|budget|run|stop]
/diff [--agent <id>]
/apply [--agent <id>]
/rollback [checkpoint]
/context [status|search <query>|reindex|inspect]
/memory
/knowledge [list|show|suggest|approve|reject|edit] [id]
/playbook [list|show|run|validate] [name]
/trace [show|export]
/insights [show|analyze|proposals]
/handoff local|daemon|remote [target]
/takeover terminal|browser|desktop|mobile
/control-return
/computer [status|observe|record|test]
/jobs [list|show|cancel|logs] [id]
/mcp [list|add|remove|auth|doctor]
/permissions
/plugins [list|install|remove|permissions]
/policy [explain|check]
/sandbox [doctor]
/resume [session]
/fork
/rewind [seq]
/compact";

/// Parse a composer line into a typed command.
///
/// Input that does not start with `/` is [`CommandError::NotACommand`] so the
/// caller can submit it as a prompt. Unknown names and invalid arguments
/// return [`CommandError`] with [`CommandError::help`].
pub fn parse_command(input: &str) -> Result<UiCommand, CommandError> {
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
        Some("agents") => parse_agents(&args),
        Some("diff") => parse_diff(&args),
        Some("apply") => parse_apply(&args),
        Some("rollback") => parse_rollback(&args),
        Some("context") => parse_context(&args),
        Some("memory") => expect_none("memory", &args, UiCommand::OpenMemory),
        Some("knowledge") => parse_knowledge(&args),
        Some("playbook") => parse_playbook(&args),
        Some("trace") => parse_trace(&args),
        Some("insights") => parse_insights(&args),
        Some("handoff") => parse_handoff(&args),
        Some("takeover") => parse_takeover(&args),
        Some("control-return") => expect_none("control-return", &args, UiCommand::ControlReturn),
        Some("computer") => parse_computer(&args),
        Some("jobs") => parse_jobs(&args),
        Some("mcp") => parse_mcp(&args),
        Some("permissions") => expect_none("permissions", &args, UiCommand::OpenPermissions),
        Some("plugins") => parse_plugins(&args),
        Some("policy") => parse_policy(&args),
        Some("sandbox") => parse_sandbox(&args),
        Some("model") => parse_model(&args),
        Some("resume") => parse_resume(&args),
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
        UiCommand::AgentsList | UiCommand::AgentsShow { .. } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Agents))
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
            FrontendAction::Local(LocalAction::Open(Inspector::Context))
        }
        UiCommand::ContextSearch { .. } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Context))
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
        UiCommand::JobsList | UiCommand::JobsShow { .. } | UiCommand::JobsLogs { .. } => {
            FrontendAction::Local(LocalAction::Open(Inspector::Jobs))
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
            Self::Empty | Self::TooLong | Self::NotACommand | Self::UnknownCommand => CATALOG_HELP,
            Self::InvalidArgs { command } | Self::InvalidId { field: command } => {
                usage_for(command).unwrap_or(CATALOG_HELP)
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
            Self::Agents => Some(UiRoute::Agents),
            Self::Diff { .. } => Some(UiRoute::Diff),
            Self::Context => Some(UiRoute::Context),
            Self::Memory => Some(UiRoute::Memory),
            Self::Jobs => Some(UiRoute::Jobs),
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

fn parse_agents(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::AgentsList),
        ["show", rest @ ..] => Ok(UiCommand::AgentsShow {
            id: optional_id("agents", rest)?,
        }),
        ["pause", rest @ ..] => Ok(UiCommand::AgentsPause {
            id: optional_id("agents", rest)?,
        }),
        ["resume", rest @ ..] => Ok(UiCommand::AgentsResume {
            id: optional_id("agents", rest)?,
        }),
        ["sleep", rest @ ..] => Ok(UiCommand::AgentsSleep {
            id: optional_id("agents", rest)?,
        }),
        ["cancel", rest @ ..] => Ok(UiCommand::AgentsCancel {
            id: optional_id("agents", rest)?,
        }),
        ["terminate", rest @ ..] => Ok(UiCommand::AgentsTerminate {
            id: optional_id("agents", rest)?,
        }),
        _ => Err(invalid("agents")),
    }
}

fn parse_diff(args: &[&str]) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::OpenDiff {
        agent: optional_agent_flag("diff", args)?,
    })
}

fn parse_apply(args: &[&str]) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::Apply {
        agent: optional_agent_flag("apply", args)?,
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

fn parse_knowledge(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::KnowledgeList),
        ["show", rest @ ..] => Ok(UiCommand::KnowledgeShow {
            id: optional_id("knowledge", rest)?,
        }),
        ["suggest"] => Err(invalid("knowledge")),
        ["suggest", rest @ ..] => Ok(UiCommand::KnowledgeSuggest {
            text: join_text("knowledge", rest)?,
        }),
        ["approve", rest @ ..] => Ok(UiCommand::KnowledgeApprove {
            id: require_id("knowledge", rest)?,
        }),
        ["reject", rest @ ..] => Ok(UiCommand::KnowledgeReject {
            id: require_id("knowledge", rest)?,
        }),
        ["edit", rest @ ..] => Ok(UiCommand::KnowledgeEdit {
            id: require_id("knowledge", rest)?,
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

fn parse_jobs(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(UiCommand::JobsList),
        ["show", rest @ ..] => Ok(UiCommand::JobsShow {
            id: optional_id("jobs", rest)?,
        }),
        ["cancel", rest @ ..] => Ok(UiCommand::JobsCancel {
            id: optional_id("jobs", rest)?,
        }),
        ["logs", rest @ ..] => Ok(UiCommand::JobsLogs {
            id: optional_id("jobs", rest)?,
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

fn parse_resume(args: &[&str]) -> Result<UiCommand, CommandError> {
    Ok(UiCommand::Resume {
        session: optional_id("resume", args)?,
    })
}

fn parse_rewind(args: &[&str]) -> Result<UiCommand, CommandError> {
    match args {
        [] => Ok(UiCommand::Rewind { to_seq: None }),
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
        return Some(CATALOG_HELP);
    }
    CATALOG
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .map(|spec| spec.usage)
}

fn help_for(topic: Option<&str>) -> InlineHelp {
    let usage = topic.and_then(usage_for).unwrap_or(CATALOG_HELP).to_owned();
    InlineHelp {
        topic: topic.map(str::to_owned),
        usage,
    }
}

fn optional_agent_flag(
    command: &'static str,
    args: &[&str],
) -> Result<Option<AgentId>, CommandError> {
    match args {
        [] => Ok(None),
        ["--agent", raw] => parse_typed_id(command, raw).map(Some),
        _ => Err(invalid(command)),
    }
}

fn optional_id<T: FromStr<Err = IdParseError>>(
    command: &'static str,
    args: &[&str],
) -> Result<Option<T>, CommandError> {
    match args {
        [] => Ok(None),
        [raw] => parse_typed_id(command, raw).map(Some),
        _ => Err(invalid(command)),
    }
}

fn require_id<T: FromStr<Err = IdParseError>>(
    command: &'static str,
    args: &[&str],
) -> Result<T, CommandError> {
    match optional_id(command, args)? {
        Some(id) => Ok(id),
        None => Err(invalid(command)),
    }
}

fn parse_typed_id<T: FromStr<Err = IdParseError>>(
    field: &'static str,
    raw: &str,
) -> Result<T, CommandError> {
    T::from_str(raw).map_err(|_| CommandError::InvalidId { field })
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
    const GOLDEN_HELP: &str = CATALOG_HELP;

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

    #[test]
    fn inspectors_are_local_chrome() {
        assert_eq!(
            dispatch(parse_ok("/agents")),
            FrontendAction::Local(LocalAction::Open(Inspector::Agents))
        );
        assert_eq!(Inspector::Agents.route(), Some(UiRoute::Agents));
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
        assert_eq!(GOLDEN_HELP, CATALOG_HELP);
        assert!(GOLDEN_HELP.contains("/control-return"));
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
