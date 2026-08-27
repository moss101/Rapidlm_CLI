//! Context-owning live-recovery host (P5-024 production wiring).
//!
//! This is the composition-root host that owns a reusable live model context and
//! runs the recovery-capable executor. It couples:
//!
//!   - a live [`ContextPacket`] (the single Context-Fabric authority), rebuilt
//!     via `context_engine::compile` + `compact_packet` on overflow;
//!   - a [`LiveModelCall`] backing (the provider adapter) that reads that packet;
//!   - the existing [`ContextController`]/[`ContextRetryPolicy`] recovery seam.
//!
//! It does NOT become the Policy Engine, Capability Broker, Workspace Manager or
//! Model Provider; those remain separate authorities. It preserves the Goal +
//! completion criteria, AGENTS/rules, selected skills, workspace/evidence/artifact
//! refs and the output-token reserve across a rebuild, and never replays committed
//! tool effects (the executor's recovery loop fails closed when `tool_calls > 0`).

use std::cell::RefCell;
use std::rc::Rc;

use agent_runtime::{
    AgentExecutionError, AgentExecutionRequest, AgentExecutor, AgentResult, CancellationToken,
    ContextController, ContextOverflow, ContextRecoveryDecision, ContextRetryPolicy,
    ContextRevision, ModelDriver, ModelStepError, ModelStepInput, ModelStepOutput,
    ProposedToolCall, ToolDriver, ToolStepError, ToolStepResult, TurnAgentExecutor, TurnEventSink,
    ValidatedToolCall,
};
use context_engine::CancellationToken as CeCancel;
use context_engine::compact::compact_packet;
use context_engine::compile::{
    CompileContext, CompileError, CompileInput, ContextBlock, ContextPacket, compile,
};
use protocol::{ArtifactId, ArtifactRef, EvidenceId, WorkspaceViewId};

/// Hard byte cap for the goal statement.
pub const MAX_GOAL_BYTES: usize = 16 * 1024;
/// Maximum retained completion criteria.
pub const MAX_CRITERIA: usize = 64;
/// Byte cap for one criterion text.
pub const MAX_CRITERION_BYTES: usize = 4 * 1024;
/// Byte cap for composed AGENTS/rules text.
pub const MAX_RULES_BYTES: usize = 32 * 1024;
/// Byte cap for composed selected-skills text.
pub const MAX_SKILLS_BYTES: usize = 16 * 1024;
/// Maximum retained evidence ids.
pub const MAX_EVIDENCE: usize = 64;
/// Maximum retained artifact refs.
pub const MAX_ARTIFACTS: usize = 64;
/// Byte cap for a compaction summary block.
pub const MAX_COMPACTION_SUMMARY: usize = 8 * 1024;
/// Byte cap for the rendered active-reminders block.
pub const MAX_REMINDERS_BLOCK_BYTES: usize = 8 * 1024;

/// Typed host-construction failure. Display never echoes block text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostError {
    InvalidPreserved,
    Compile(CompileError),
}

/// State preserved across a live-context rebuild. Authority over the actual
/// context stays with the Context Fabric; this only carries what a rebuild must
/// not lose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreservedLiveContext {
    goal_statement: String,
    completion_criteria: Vec<String>,
    agents_rules: String,
    skills: String,
    workspace_view: Option<WorkspaceViewId>,
    evidence: Vec<EvidenceId>,
    artifacts: Vec<ArtifactRef>,
    context_limit: u32,
    output_reserve: u32,
    reminders_block: Option<String>,
}

impl PreservedLiveContext {
    pub fn new(
        goal_statement: impl Into<String>,
        completion_criteria: Vec<String>,
        agents_rules: impl Into<String>,
        skills: impl Into<String>,
        context_limit: u32,
        output_reserve: u32,
    ) -> Result<Self, HostError> {
        let goal_statement = goal_statement.into();
        let agents_rules = agents_rules.into();
        let skills = skills.into();
        if goal_statement.is_empty()
            || goal_statement.len() > MAX_GOAL_BYTES
            || completion_criteria.len() > MAX_CRITERIA
            || completion_criteria
                .iter()
                .any(|c| c.is_empty() || c.len() > MAX_CRITERION_BYTES)
            || agents_rules.len() > MAX_RULES_BYTES
            || skills.len() > MAX_SKILLS_BYTES
            || context_limit == 0
        {
            return Err(HostError::InvalidPreserved);
        }
        Ok(Self {
            goal_statement,
            completion_criteria,
            agents_rules,
            skills,
            workspace_view: None,
            evidence: Vec::new(),
            artifacts: Vec::new(),
            context_limit,
            output_reserve,
            reminders_block: None,
        })
    }

    pub fn with_workspace_view(mut self, view: WorkspaceViewId) -> Self {
        self.workspace_view = Some(view);
        self
    }

    /// Attach the rendered active-reminders block for this turn. Empty
    /// blocks are ignored (no block, no packet entry).
    pub fn with_reminders_block(mut self, block: Option<String>) -> Self {
        let within_bounds = block
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.len() <= MAX_REMINDERS_BLOCK_BYTES);
        if within_bounds {
            self.reminders_block = block;
        }
        self
    }

    pub fn reminders_block(&self) -> Option<&str> {
        self.reminders_block.as_deref()
    }

    pub fn with_evidence(mut self, evidence: Vec<EvidenceId>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactRef>) -> Result<Self, HostError> {
        if artifacts.len() > MAX_ARTIFACTS {
            return Err(HostError::InvalidPreserved);
        }
        self.artifacts = artifacts;
        Ok(self)
    }
}

/// The live in-flight model context the host owns and can rebuild.
#[derive(Clone, Debug)]
pub struct LiveContext {
    packet: ContextPacket,
    revision: ArtifactId,
    preserved: PreservedLiveContext,
}

impl LiveContext {
    pub fn packet(&self) -> &ContextPacket {
        &self.packet
    }
    pub fn revision(&self) -> ArtifactId {
        self.revision
    }
    pub fn preserved(&self) -> &PreservedLiveContext {
        &self.preserved
    }
}

/// Provider-adapter contract: produce one model step from the live context.
pub trait LiveModelCall {
    fn step(
        &mut self,
        blocks: &[ContextBlock],
        prior_tools: &[ToolStepResult],
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError>;
}

/// A [`ModelDriver`] bound to the host-owned live context. Reads the (possibly
/// rebuilt) packet on every step, so overflow recovery changes what it sees.
pub struct LiveContextModelDriver<B> {
    live: Rc<RefCell<LiveContext>>,
    backing: B,
}

impl<B: LiveModelCall> ModelDriver for LiveContextModelDriver<B> {
    fn step(
        &mut self,
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            return Err(ModelStepError::Cancelled);
        }
        let live = self.live.borrow();
        self.backing
            .step(live.packet().blocks(), input.prior_tools(), cancel)
    }
}

/// Rebuilds the live context via the Context Fabric on a typed overflow.
pub struct LiveRecoveryController {
    live: Rc<RefCell<LiveContext>>,
    summary: Option<String>,
}

impl LiveRecoveryController {
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }
}

impl ContextController for LiveRecoveryController {
    fn recover_from_overflow(&mut self, _request: ContextOverflow) -> ContextRecoveryDecision {
        // Compact via the Context-Fabric summarization policy (never arbitrary
        // truncation); mandatory/system blocks are retained by the pack summary.
        {
            let live = self.live.borrow();
            let compacted = match compact_packet(live.packet(), None, &CeCancel::new()) {
                Ok(compacted) => compacted,
                Err(_) => return ContextRecoveryDecision::NotRecoverable,
            };
            let summary = compacted.summary().to_owned();
            if summary.is_empty() || summary.len() > MAX_COMPACTION_SUMMARY {
                self.summary = None;
            } else {
                self.summary = Some(summary);
            }
        }
        let new_packet = {
            let live = self.live.borrow();
            match build_packet(live.preserved(), self.summary.as_deref()) {
                Ok(packet) => packet,
                Err(_) => return ContextRecoveryDecision::NotRecoverable,
            }
        };
        {
            let mut live = self.live.borrow_mut();
            live.packet = new_packet;
            live.revision = ArtifactId::from_bytes(b"context/live-recovery");
        }
        ContextRecoveryDecision::Recovered
    }

    fn rebuilt_context(&self) -> Option<ContextRevision> {
        Some(ContextRevision::new(
            "context/live-recovery",
            Some(self.live.borrow().revision()),
        ))
    }
}

/// The context-owning host. Runs the recovery-capable executor over a live
/// context it can rebuild; each rebuilt context is a new [`ContextRevision`].
pub struct LiveContextHost<B> {
    live: Rc<RefCell<LiveContext>>,
    model: LiveContextModelDriver<B>,
    controller: LiveRecoveryController,
    policy: ContextRetryPolicy,
}

impl<B: LiveModelCall> LiveContextHost<B> {
    pub fn from_preserved(
        preserved: PreservedLiveContext,
        backing: B,
        policy: ContextRetryPolicy,
    ) -> Result<Self, HostError> {
        Self::build(preserved, backing, policy)
    }

    pub fn build(
        preserved: PreservedLiveContext,
        backing: B,
        policy: ContextRetryPolicy,
    ) -> Result<Self, HostError> {
        let packet = build_packet(&preserved, None).map_err(HostError::Compile)?;
        let revision = ArtifactId::from_bytes(b"context/live");
        let live = Rc::new(RefCell::new(LiveContext {
            packet,
            revision,
            preserved,
        }));
        let model = LiveContextModelDriver {
            live: Rc::clone(&live),
            backing,
        };
        let controller = LiveRecoveryController {
            live: Rc::clone(&live),
            summary: None,
        };
        Ok(Self {
            live,
            model,
            controller,
            policy,
        })
    }

    /// Run one agent turn through the recovery-capable executor.
    pub fn execute<T, E>(
        &mut self,
        request: &AgentExecutionRequest,
        tools: &mut T,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> Result<AgentResult, AgentExecutionError>
    where
        T: ToolDriver,
        E: TurnEventSink,
    {
        TurnAgentExecutor.execute_with_context_recovery(
            request,
            &mut self.controller,
            self.policy,
            &mut self.model,
            tools,
            events,
            cancel,
        )
    }

    pub fn live_context(&self) -> &Rc<RefCell<LiveContext>> {
        &self.live
    }

    pub fn policy(&self) -> ContextRetryPolicy {
        self.policy
    }
}

impl From<CompileError> for HostError {
    fn from(err: CompileError) -> Self {
        Self::Compile(err)
    }
}

/// A [`ToolDriver`] with no tool gateway configured. Structural tool calls are
/// never executed; used by the bare CLI `exec` entry that has no tool wiring.
pub struct NoopTools;

impl ToolDriver for NoopTools {
    fn validate(
        &mut self,
        _call: &ProposedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        Err(ToolStepError::Invalid)
    }
    fn execute(
        &mut self,
        _call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        Err(ToolStepError::Invalid)
    }
}

/// A [`LiveModelCall`] that has no provider configured. A real llm-router
/// adapter replaces this in production; without it a model step is a typed
/// provider failure (never a panic, never synthetic completion).
pub struct UnconfiguredModel;

impl LiveModelCall for UnconfiguredModel {
    fn step(
        &mut self,
        _blocks: &[ContextBlock],
        _prior_tools: &[ToolStepResult],
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            Err(ModelStepError::Cancelled)
        } else {
            Err(ModelStepError::Failed)
        }
    }
}

/// Production entry used by the CLI `exec`/`goal` command: build the
/// context-owning host and run one agent turn through the recovery-capable
/// executor. A real provider adapter is injected as the `backing`.
pub fn run_live_exec<B, T, E>(
    preserved: PreservedLiveContext,
    backing: B,
    request: &AgentExecutionRequest,
    tools: &mut T,
    events: &mut E,
    cancel: &CancellationToken,
    policy: ContextRetryPolicy,
) -> Result<AgentResult, AgentExecutionError>
where
    B: LiveModelCall,
    T: ToolDriver,
    E: TurnEventSink,
{
    let mut host = LiveContextHost::build(preserved, backing, policy)
        .map_err(|_| AgentExecutionError::InvalidRequest)?;
    host.execute(request, tools, events, cancel)
}

/// Compile a live [`ContextPacket`] from preserved state + optional compaction
/// summary. This is the single Context-Fabric rebuild path.
pub fn build_packet(
    preserved: &PreservedLiveContext,
    summary: Option<&str>,
) -> Result<ContextPacket, CompileError> {
    let mut ctx = CompileContext::new(preserved.context_limit, preserved.output_reserve)
        .task("live agent turn")
        .goal(preserved.goal_statement.clone());
    if !preserved.agents_rules.is_empty() {
        ctx = ctx.system(CompileInput::new(
            "rules/agents",
            preserved.agents_rules.clone(),
        ));
    }
    if !preserved.skills.is_empty() {
        ctx = ctx.system(CompileInput::new(
            "skills/selected",
            preserved.skills.clone(),
        ));
    }
    for criterion in &preserved.completion_criteria {
        ctx = ctx.goal_block(CompileInput::new("criterion", criterion.clone()));
    }
    if let Some(summary) = summary
        && !summary.is_empty() {
            ctx = ctx.memory(CompileInput::new("context/compaction", summary.to_owned()));
        }
    if let Some(reminders) = preserved.reminders_block() {
        ctx = ctx.system(CompileInput::new("reminders/active", reminders.to_owned()));
    }
    compile(&ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::{
        AgentRole, AgentSpec, ModelStepOutput, ProposedToolCall, ToolStepError, ToolStepResult,
        ValidatedToolCall,
    };
    use protocol::{AgentId, SessionId};
    use std::collections::VecDeque;

    struct ScriptedBacking {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
        saw_blocks: Vec<usize>,
    }
    impl ScriptedBacking {
        fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
            Self {
                outputs: outputs.into(),
                saw_blocks: Vec::new(),
            }
        }
    }
    impl LiveModelCall for ScriptedBacking {
        fn step(
            &mut self,
            blocks: &[ContextBlock],
            prior_tools: &[ToolStepResult],
            cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            if cancel.is_cancelled() {
                return Err(ModelStepError::Cancelled);
            }
            self.saw_blocks.push(blocks.len());
            if prior_tools.is_empty() {
                // ignored; scripted outputs drive the scenario
            }
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    struct CountingTools {
        executed: u32,
    }
    impl ToolDriver for CountingTools {
        fn validate(
            &mut self,
            call: &agent_runtime::ProposedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            Ok(ValidatedToolCall::from_proposed(call))
        }
        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ToolStepResult, ToolStepError> {
            self.executed += 1;
            Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: "ok".to_owned(),
            })
        }
    }

    fn preserved() -> PreservedLiveContext {
        PreservedLiveContext::new(
            "implement feature X",
            vec!["tests pass".to_owned()],
            "trusted AGENTS rules".to_owned(),
            "selected skill".to_owned(),
            8192,
            256,
        )
        .expect("preserved")
    }

    fn spec() -> AgentSpec {
        AgentSpec::builder(
            AgentId::new(),
            AgentRole::Coder,
            "implement",
            WorkspaceViewId::new(),
        )
        .permissions_profile("work")
        .build()
        .expect("spec")
    }

    fn overflow_then_terminal(text: &str) -> ScriptedBacking {
        ScriptedBacking::new(vec![
            Err(ModelStepError::BoundExceeded),
            Ok(ModelStepOutput::Terminal {
                text: text.to_owned(),
                tokens: 1,
            }),
        ])
    }

    fn run_session(
        mut host: LiveContextHost<ScriptedBacking>,
    ) -> Result<AgentResult, AgentExecutionError> {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        host.execute(
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
        )
    }

    #[test]
    fn reminders_block_is_compiled_into_the_packet_as_a_system_block() {
        let block = "reminders schema=rapidlm.reminders.v1 feeds=ops\n\
                     [ops/budget-guard floor=medium] stay under the time budget\n";
        let with_reminders = preserved().with_reminders_block(Some(block.to_owned()));
        let packet = build_packet(&with_reminders, None).expect("packet");
        let reminders = packet
            .blocks()
            .iter()
            .find(|b| b.text().contains("budget-guard"))
            .expect("reminders block in packet");
        assert_eq!(reminders.source(), context_engine::compile::ContextSource::System);
        // Out-of-bounds blocks are refused: nothing enters the packet.
        let oversized = "x".repeat(MAX_REMINDERS_BLOCK_BYTES + 1);
        let with_oversized = preserved().with_reminders_block(Some(oversized));
        let packet = build_packet(&with_oversized, None).expect("packet");
        assert!(packet.blocks().iter().all(|b| !b.text().contains("reminders schema")));
        // None adds no block.
        let packet = build_packet(&preserved(), None).expect("packet");
        assert!(packet.blocks().iter().all(|b| !b.text().contains("reminders schema")));
    }

    #[test]
    fn overflow_rebuilds_live_context_and_recovers() {
        let host = LiveContextHost::build(
            preserved(),
            overflow_then_terminal("after rebuild"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let result = run_session(host).expect("execute");
        assert_eq!(
            result.status(),
            agent_runtime::AgentTerminalStatus::Succeeded
        );
        assert_eq!(result.summary(), "after rebuild");
        // Lineage records the recovery transition without raw context contents.
        assert_eq!(result.context_lineage().len(), 1);
        assert_eq!(
            result.context_lineage()[0].source(),
            "context/live-recovery"
        );
    }

    #[test]
    fn normal_context_succeeds_without_recovery() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![Ok(ModelStepOutput::Terminal {
                text: "done".to_owned(),
                tokens: 1,
            })]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let result = run_session(host).expect("execute");
        assert_eq!(result.summary(), "done");
        assert!(result.context_lineage().is_empty());
    }

    #[test]
    fn repeated_overflow_fails_closed_with_typed_limit() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![
                Err(ModelStepError::BoundExceeded),
                Err(ModelStepError::BoundExceeded),
                Err(ModelStepError::BoundExceeded),
            ]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let err = run_session(host).expect_err("bounded stop");
        assert_eq!(err, AgentExecutionError::ContextRetryExceeded);
    }

    #[test]
    fn provider_failure_does_not_trigger_compaction() {
        let host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![Err(ModelStepError::Failed)]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        // Provider failure is typed `ModelFailed`, never `ContextBoundExceeded`,
        // so the recovery controller is not consulted (no compaction).
        let result = run_session(host).expect("execute");
        assert_eq!(result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert!(result.context_lineage().is_empty());
    }

    #[test]
    fn committed_tool_effects_are_not_replayed_on_overflow() {
        let call = ProposedToolCall::new("c1", "repo.read", "{}").expect("call");
        let mut host = LiveContextHost::build(
            preserved(),
            ScriptedBacking::new(vec![
                Ok(ModelStepOutput::ToolCalls {
                    calls: vec![call],
                    tokens: 1,
                }),
                Err(ModelStepError::BoundExceeded),
            ]),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut tools = CountingTools { executed: 0 };
        let mut events = Vec::new();
        let err = host
            .execute(&request, &mut tools, &mut events, &CancellationToken::new())
            .expect_err("fail closed");
        assert_eq!(err, AgentExecutionError::ContextOverflowAfterEffects);
        assert_eq!(tools.executed, 1, "effect executed once, never replayed");
    }

    #[test]
    fn cancellation_during_recovery_stops_execution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut host = LiveContextHost::build(
            preserved(),
            overflow_then_terminal("never"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let err = host
            .execute(
                &request,
                &mut CountingTools { executed: 0 },
                &mut events,
                &cancel,
            )
            .expect_err("cancelled");
        assert_eq!(err, AgentExecutionError::Cancelled);
    }

    #[test]
    fn preserved_goal_rules_skills_survive_rebuild() {
        let preserved = preserved();
        let host = LiveContextHost::build(
            preserved.clone(),
            overflow_then_terminal("ok"),
            ContextRetryPolicy::new(2),
        )
        .expect("host");
        let live = host.live_context().borrow();
        let p = live.preserved();
        assert_eq!(p.goal_statement, "implement feature X");
        assert_eq!(p.completion_criteria, vec!["tests pass".to_owned()]);
        assert_eq!(p.agents_rules, "trusted AGENTS rules");
        assert_eq!(p.skills, "selected skill");
    }

    #[test]
    fn cli_host_entry_reaches_recovery_capable_executor() {
        // Drives the exact entry a CLI `exec`/`goal` command uses (run_live_exec):
        // a goal/turn reaches the recovery-capable executor end-to-end.
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let result = run_live_exec(
            preserved(),
            overflow_then_terminal("wired recovery"),
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
        )
        .expect("execute");
        assert_eq!(result.summary(), "wired recovery");
        assert_eq!(result.context_lineage().len(), 1);
        assert_eq!(
            result.context_lineage()[0].source(),
            "context/live-recovery"
        );
    }

    #[test]
    fn unconfigured_provider_is_a_typed_failure_not_synthetic_completion() {
        let request = AgentExecutionRequest::new(spec(), SessionId::new());
        let mut events = Vec::new();
        let result = run_live_exec(
            preserved(),
            UnconfiguredModel,
            &request,
            &mut CountingTools { executed: 0 },
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
        )
        .expect("execute");
        assert_eq!(result.status(), agent_runtime::AgentTerminalStatus::Failed);
        assert!(result.context_lineage().is_empty(), "no fake recovery");
    }
}
