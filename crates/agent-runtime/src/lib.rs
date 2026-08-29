#![forbid(unsafe_code)]

pub mod agent {
    pub mod model;
    pub mod ops;
    pub mod result;
    pub mod scheduler;
    pub mod spawn;
}

pub mod agent_executor;

pub mod agent_defs;
pub mod compaction;
pub mod context_recovery;
pub mod delegation;
pub mod evidence;
pub mod loop_guard;
pub mod reminders;
pub mod role_profile;
pub mod rules_loader;
pub mod specialist;

pub mod goal {
    pub mod budget;
    pub mod driver;
    pub mod recovery;
    pub mod state;
}

pub use agent_executor::{
    AgentExecutionError, AgentExecutionRequest, AgentExecutor, AgentOutcome,
    DEFAULT_RECOVERY_SOURCE, TurnAgentExecutor,
};
pub use context_recovery::{
    ContextController, ContextOverflow, ContextRecoveryDecision, ContextRecoveryError,
    ContextRetryPolicy, RetryOutcome, should_retry,
};
pub use delegation::{
    DELEGATION_DEFAULT_MAX_CONFLICT_RISK, DELEGATION_DEFAULT_MAX_SPAWN_COST,
    DELEGATION_DEFAULT_THRESHOLD, DELEGATION_MAX_DEPTH, DelegationInput, DelegationPolicy,
    DelegationScore, EnforcementDecision, ScoringRecommendation, enforce_delegation,
    evaluate_delegation, recommend,
};
pub use loop_guard::{
    DEFAULT_REPEATED_MESSAGE_THRESHOLD, DEFAULT_REPEATED_MESSAGE_WINDOW,
    DEFAULT_REPEATED_TOOL_CALL_THRESHOLD, DEFAULT_REPEATED_TOOL_CALL_WINDOW, MessageLoopDetector,
    MessageSignature, ToolCallLoopDetector, ToolCallSignature,
};
pub use role_profile::{
    RoleModelPolicy, RoleProfile, RoleRegistry, RoleToolClass, RoleToolSurface,
};
pub use prompt_stack::{
    MAX_ENVIRONMENT_BYTES, MAX_SYSTEM_PROMPT_BYTES, POST_COMPACTION_SYSTEM_PROMPT,
    PromptContext, PromptStackError, TrustPosture, render_system_prompt,
};
pub use rules_loader::{
    AGENTS_FILE, COMPAT_RULES_DIRS, INSTRUCTION_FILE_NAMES, MAX_AGENTS_BYTES, MAX_AGENTS_FILES,
    RuleEntry, RulesBundle, RulesError, discover_instructions, load_agents,
};
pub use specialist::{
    DEFAULT_SPECIALIST_MAILBOX, DEFAULT_SPECIALIST_SUMMARY_BYTES, PersistentSpecialist,
    SpecialistConfig, SpecialistError, SpecialistMessage, SpecialistPool,
};

pub mod orchestration;
pub mod prompt;
pub mod prompt_stack;
pub mod turn;

pub use agent::model::{
    AGENT_RESULT_SCHEMA, AGENT_SCHEMA_VERSION, AGENT_SPEC_SCHEMA, Agent, AgentBudget,
    AgentModelError, AgentResult, AgentRole, AgentSpec, AgentSpecBuilder, AgentState, AgentStats,
    AgentTerminalStatus, CancellationToken, ContextRevision, IdentityField, MAX_ARTIFACTS,
    MAX_EVIDENCE, MAX_PERMISSIONS_PROFILE_BYTES, MAX_SUMMARY_BYTES, MAX_TASK_BYTES, ModelPolicyRef,
    PatchSummary, validate_identity, validate_transition,
};
pub use agent::result::{
    InspectedResult, MAX_RESULT_EVENTS, MAX_STORED_RESULTS, MergeEnv, MergeHandoff, MergeRequest,
    MergeStatus, ResultError, ResultEvent, ResultEventKind, ResultEventSink, ResultStore,
    complete_agent, inspect_result, merge_result,
};
pub use agent::scheduler::{
    AgentHandle, AgentPriority, LimitKind, MAX_PROVIDER_KEY_BYTES, MAX_RESULT_SCHEMA_BYTES,
    MAX_SCHEDULED_AGENTS, ProviderKey, ResultSchema, ScheduleState, ScheduledAgent, Scheduler,
    SchedulerError, SchedulerLimits, SchedulerOccupancy, SpawnAgent, SpawnAgentBuilder,
    WorkspaceAccess,
};
pub use agent::spawn::{
    ChildPrincipal, IsolatedWorktree, MAX_SPAWN_EVENTS, ParentLeaseTransfer, SpawnEnv, SpawnError,
    SpawnEvent, SpawnEventKind, SpawnEventSink, SpawnRequest, SpawnRequestBuilder, SpawnedAgent,
    spawn_agent,
};
pub use compaction::{
    COMPACTION_ARTIFACT_SCHEMA, COMPACTION_EVENT_KIND, COMPACTION_EVENT_SCHEMA,
    COMPACTION_MEDIA_TYPE, COMPACTION_SCHEMA_VERSION, ChangedFile, CompactionArtifact,
    CompactionDecision, CompactionEffect, CompactionError, CompactionEvent, CompactionEventKind,
    CompactionHandles, CompactionInput, CompactionLimits, Compactor, DEFAULT_COMPACT_TIMEOUT,
    DEFAULT_MAX_ARTIFACT_BYTES, DEFAULT_MAX_BLOCKERS, DEFAULT_MAX_CONSTRAINTS,
    DEFAULT_MAX_DECISIONS, DEFAULT_MAX_EVIDENCE, DEFAULT_MAX_FILES, DEFAULT_MAX_HANDLES,
    DEFAULT_MAX_QUESTIONS, DEFAULT_MAX_READ_HASHES, DEFAULT_MAX_SYMBOLS, DEFAULT_MAX_TEXT_BYTES,
    EventRange, ReadHash, UnresolvedBlocker, compact,
};
pub use evidence::{
    BackingError, BackingResolver, COMPLETION_CHECK_SCHEMA, CRITERION_VERDICTS_SCHEMA,
    CompletionCheck, CriterionEvaluator, CriterionUnsatisfied, CriterionVerdict,
    CriterionVerdicts, EVIDENCE_RECORD_SCHEMA, EVIDENCE_SCHEMA_VERSION, EvidenceError,
    EvidenceEventKind, EvidenceFreshness, EvidenceKind, EvidenceLedgerRef, EvidenceProducer,
    EvidenceRecord, EvidenceService, EvidenceSource, EvidenceSpec, EvidenceStatus, EvidenceStore,
    MAX_ASSERTION_BYTES, MAX_COMMAND_BYTES, MAX_EVIDENCE_RECORDS, MAX_EVENT_REF_BYTES,
    MAX_SUBJECT_BYTES, SharedBackingResolver, TEST_PASSED,
};
pub use goal::budget::{
    BudgetDimension, CONVERGENCE_DENOMINATOR, CONVERGENCE_NUMERATOR, ConvergenceHint,
    GoalBudgetError, GoalBudgetGuard, GoalBudgetOutcome,
};
pub use goal::driver::{
    GOAL_BLOCK_TOOL, GOAL_CANCEL_TOOL, GOAL_COMPLETE_TOOL, GOAL_PAUSE_TOOL, GoalDriver,
    GoalDriverError, GoalDriverOutcome, GoalDriverStop, GoalSession,
};
pub use goal::recovery::{GoalRecovery, GoalRecoveryError, recover_goal, recover_goal_with_cancel};
pub use goal::state::{
    Criterion, EvidenceRequirement, GOAL_SCHEMA_VERSION, GOAL_SNAPSHOT_SCHEMA, GoalActor,
    GoalBudget, GoalCommand, GoalCommandKind, GoalEffect, GoalEventKind, GoalSnapshot, GoalSpec,
    GoalState, GoalStateError, GoalStateMachine, GoalStopReason, GoalUsage, MAX_CRITERIA,
    MAX_CRITERION_TEXT_BYTES, MAX_EVIDENCE_KINDS, MAX_EVIDENCE_REQUIREMENTS,
    MAX_GOAL_STATEMENT_BYTES,
};
pub use orchestration::{
    AcceptanceCriterion, AcceptancePolicy, AgentContextPacket, CandidateCompletion, CheckResult,
    CheckRunner, CheckStatus, CompletionClaim, DiscoveryResult, EvidenceNode, EvidenceTrust,
    Explorer, GapNode, Implementer, MAX_ID_BYTES, OrchestrationBudget, OrchestrationEvidenceKind,
    OrchestrationState, Planner, PlanResult, RequirementClaim, RequirementClaimStatus,
    RequirementNode, RequirementPriority, Retriever, StrategyRevision, Strategist, Supervisor,
    SupervisorDrivers, SupervisorError, TaskContract, TaskComplexity, TransitionError,
    VerificationCheck, VerificationPolicy, VerificationVerdict, Verdict, WorkspaceIdentity,
    WorkspacePolicy,
};
pub use prompt::{
    CORE_SYSTEM_V2, CORE_SYSTEM_V2_HASH, CORE_SYSTEM_VERSION, DEFAULT_COMPILE_TIMEOUT,
    DEFAULT_MAX_CONTEXT_BLOCK_BYTES, DEFAULT_MAX_CONTEXT_BLOCKS, DEFAULT_MAX_LOCATOR_BYTES,
    DEFAULT_MAX_SECTION_BYTES, FragmentVersion, PROMPT_BUNDLE_SCHEMA, PROMPT_SCHEMA_VERSION,
    PromptBundle, PromptCompiler, PromptContextBlock, PromptError, PromptInputs, PromptLimits,
    PromptMessage, PromptSection, PromptSlot, PromptTrust, ROLE_FRAGMENT_VERSION, core_prefix,
    core_prefix_hash,
};
pub use turn::{
    BoundedAssistantOutput, FailureCause, MAX_ARGUMENT_BYTES,
    MAX_CALL_ID_BYTES, MAX_MODEL_STEPS, MAX_TEXT_BYTES, MAX_TOOL_CALLS_PER_STEP,
    MAX_TOOL_NAME_BYTES, MAX_TURN_EVENTS, ModelDriver, ModelStepError, ModelStepInput,
    ModelStepOutput, ProposedToolCall, ToolDriver, ToolKind, ToolStepError, ToolStepExchange,
    ToolStepResult, TurnBudget, TurnError, TurnEvent, TurnEventKind, TurnEventSink,
    TurnFailureDetail, TurnResult, ToolSurface, TurnSpec, TurnStatus, TurnStopReason, TurnUsage,
    ValidatedToolCall, run_turn,
};
