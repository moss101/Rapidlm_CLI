//! ExternalAgentAdapter — ACP/CLI external coding agents behind the Agent
//! node contract (P9-022/023/024).
//!
//! External agent output is untrusted context: results are bounded,
//! trust-labeled, and normalized into one typed record before any consumer
//! sees them. Process loss returns a bounded recovery result instead of an
//! error escaping the node. The only process/stdio seam is the
//! [`AgentChannel`] trait; deterministic fakes exist exclusively under test.

use std::path::PathBuf;
use std::time::Duration;

use acp::stdio::{JsonRpcId, JsonRpcMessage, StdioError};
use acp::v1::StopReason;
use capability_broker::{CancellationToken, CanonicalHostPath, LeaseUseGuard, PrincipalRef};
use process_supervisor::{
    ExecBinding, ExecSpec, SecretOrValue, StdinSpec, await_exit, spawn, DEFAULT_GRACE,
};
use protocol::SessionId;

/// Maximum prompt bytes forwarded to an external agent.
pub const MAX_AGENT_PROMPT_BYTES: usize = 256 * 1024;
/// Maximum raw stdout bytes accepted back from an external CLI agent.
pub const MAX_AGENT_RESULT_BYTES: usize = 256 * 1024;
/// Capability-broker family used when supervising external agent processes.
pub const AGENT_COMMAND_FAMILY: &str = "agent.external";
/// Every external result carries this trust label; it can never be raised.
pub const EXTERNAL_TRUST_LABEL: &str = "untrusted_context";
/// Default wall-clock ceiling for one external agent invocation.
pub const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(600);

/// Typed external-agent failure. Display never echoes prompt or output text.
#[derive(Debug)]
pub enum ExternalAgentError {
    InvalidTask,
    PromptTooLarge,
    ProtocolFault,
    ChannelClosed,
    Transport(StdioError),
    Supervised(String),
}

impl core::fmt::Display for ExternalAgentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidTask => "external agent task is invalid",
            Self::PromptTooLarge => "prompt exceeds the configured bound",
            Self::ProtocolFault => "external agent spoke an invalid protocol frame",
            Self::ChannelClosed => "external agent channel closed unexpectedly",
            Self::Transport(inner) => return inner.fmt(f),
            Self::Supervised(_) => "supervised external agent execution failed",
        })
    }
}

impl std::error::Error for ExternalAgentError {}

fn valid_text(text: &str) -> bool {
    !text.chars().any(char::is_control) || text.lines().count() > 1
}

/// Declared capabilities of an external agent. Descriptive metadata only:
/// never a capability grant and never honored without broker approval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentCapabilities {
    load_session: bool,
    max_turn_requests: u32,
    allowed_tools: Vec<String>,
}

impl ExternalAgentCapabilities {
    pub fn new(
        load_session: bool,
        max_turn_requests: u32,
        allowed_tools: Vec<String>,
    ) -> Result<Self, ExternalAgentError> {
        if allowed_tools.len() > 64 || allowed_tools.iter().any(|t| t.len() > 128) {
            return Err(ExternalAgentError::InvalidTask);
        }
        Ok(Self {
            load_session,
            max_turn_requests: max_turn_requests.clamp(1, 64),
            allowed_tools,
        })
    }

    pub fn load_session(&self) -> bool {
        self.load_session
    }

    pub fn max_turn_requests(&self) -> u32 {
        self.max_turn_requests
    }

    /// Tool names are narrowing hints; the broker still gates every call.
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }
}

/// One unit of external work behind an Agent node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentTask {
    session_id: SessionId,
    prompt: String,
}

impl ExternalAgentTask {
    pub fn new(session_id: SessionId, prompt: impl Into<String>) -> Result<Self, ExternalAgentError> {
        let prompt = prompt.into();
        if prompt.is_empty() || prompt.len() > MAX_AGENT_PROMPT_BYTES || !valid_text(&prompt) {
            return Err(ExternalAgentError::PromptTooLarge);
        }
        Ok(Self {
            session_id,
            prompt,
        })
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

/// How the external agent is driven.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentFlavor {
    /// Speaks ACP JSON-RPC over stdio frames.
    Acp,
    /// Plain CLI executor: prompt on stdin, result on stdout.
    Cli,
}

/// Normalized terminal outcome for one external invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentOutcome {
    Completed { stop_reason: String },
    Failed { reason: String },
    /// Process crash/disconnect: bounded recovery result, never a panic.
    ProcessLost,
}

/// Bounded, trust-labeled normalization of any external agent result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedAgentResult {
    flavor: AgentFlavor,
    session_id: SessionId,
    outcome: AgentOutcome,
    text: String,
    trust_label: &'static str,
}

impl NormalizedAgentResult {
    pub fn flavor(&self) -> AgentFlavor {
        self.flavor
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn outcome(&self) -> &AgentOutcome {
        &self.outcome
    }

    /// Bounded excerpt of external text. Untrusted context by construction.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn trust_label(&self) -> &'static str {
        self.trust_label
    }
}

/// P9-024: map a CLI exit to a normalized outcome.
pub fn normalize_cli_exit(ok: bool, code: Option<i32>) -> AgentOutcome {
    if ok {
        AgentOutcome::Completed {
            stop_reason: "exit_zero".to_owned(),
        }
    } else {
        AgentOutcome::Failed {
            reason: match code {
                Some(code) => format!("exit_{code}"),
                None => "signaled".to_owned(),
            },
        }
    }
}

/// P9-024: map an ACP stop reason to a normalized outcome.
pub fn normalize_acp_stop(stop: StopReason) -> AgentOutcome {
    let reason = match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
    };
    match stop {
        StopReason::EndTurn => AgentOutcome::Completed {
            stop_reason: reason.to_owned(),
        },
        StopReason::Cancelled => AgentOutcome::Failed {
            reason: reason.to_owned(),
        },
        _ => AgentOutcome::Failed {
            reason: reason.to_owned(),
        },
    }
}

/// Bound and label one raw external text payload.
fn normalize_result(
    flavor: AgentFlavor,
    session_id: SessionId,
    outcome: AgentOutcome,
    raw_text: &str,
) -> NormalizedAgentResult {
    let mut text = raw_text.to_owned();
    if text.len() > MAX_AGENT_RESULT_BYTES {
        text.truncate(MAX_AGENT_RESULT_BYTES);
    }
    NormalizedAgentResult {
        flavor,
        session_id,
        outcome,
        text,
        trust_label: EXTERNAL_TRUST_LABEL,
    }
}

/// Build the ACP `initialize` request (client -> external agent).
pub fn acp_initialize_request(id: i64, client_name: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request {
        id: JsonRpcId::Number(id),
        method: "initialize".to_owned(),
        params: Some(serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "name": client_name,
                "version": "1"
            }
        })),
    }
}

/// Build the ACP `session/prompt` request with a bounded text block.
pub fn acp_prompt_request(
    id: i64,
    session_key: &str,
    task: &ExternalAgentTask,
) -> Result<JsonRpcMessage, ExternalAgentError> {
    if session_key.is_empty() || session_key.len() > 128 {
        return Err(ExternalAgentError::ProtocolFault);
    }
    Ok(JsonRpcMessage::Request {
        id: JsonRpcId::Number(id),
        method: "session/prompt".to_owned(),
        params: Some(serde_json::json!({
            "sessionId": session_key,
            "prompt": [{ "type": "text", "text": task.prompt() }]
        })),
    })
}

/// Parse the `initialize` response. Any shape but a successful result is a
/// protocol fault.
pub fn parse_acp_initialize(bytes: &[u8]) -> Result<String, ExternalAgentError> {
    let message: JsonRpcMessage =
        serde_json::from_slice(bytes).map_err(|_| ExternalAgentError::ProtocolFault)?;
    match message {
        JsonRpcMessage::Result { .. } => {
            // Session key is negotiated per-prompt below; presence is enough.
            Ok("initialized".to_owned())
        }
        _ => Err(ExternalAgentError::ProtocolFault),
    }
}

/// P9-022 protocol layer: parse one ACP `session/prompt` response into the
/// normalized result. Malformed payloads are protocol faults, never panics.
pub fn parse_acp_prompt_result(
    session_id: SessionId,
    bytes: &[u8],
) -> Result<NormalizedAgentResult, ExternalAgentError> {
    let message: JsonRpcMessage =
        serde_json::from_slice(bytes).map_err(|_| ExternalAgentError::ProtocolFault)?;
    match message {
        JsonRpcMessage::Result { result, .. } => {
            let stop = result
                .get("stopReason")
                .and_then(serde_json::Value::as_str)
                .ok_or(ExternalAgentError::ProtocolFault)?;
            let outcome = match stop {
                "end_turn" => normalize_acp_stop(StopReason::EndTurn),
                "max_tokens" => normalize_acp_stop(StopReason::MaxTokens),
                "max_turn_requests" => normalize_acp_stop(StopReason::MaxTurnRequests),
                "refusal" => normalize_acp_stop(StopReason::Refusal),
                "cancelled" => normalize_acp_stop(StopReason::Cancelled),
                _ => return Err(ExternalAgentError::ProtocolFault),
            };
            let text = result
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            Ok(normalize_result(AgentFlavor::Acp, session_id, outcome, text))
        }
        JsonRpcMessage::Error { .. } => Ok(normalize_result(
            AgentFlavor::Acp,
            session_id,
            AgentOutcome::Failed {
                reason: "agent_error".to_owned(),
            },
            "",
        )),
        _ => Err(ExternalAgentError::ProtocolFault),
    }
}

/// Byte-level process boundary. Production implementations drive real child
/// processes; deterministic fakes live only under `#[cfg(test)]`.
pub trait AgentChannel {
    /// Send one JSON-RPC frame and read the next response frame.
    fn exchange(
        &mut self,
        request: &JsonRpcMessage,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, ExternalAgentError>;
}

/// Drive an external ACP agent through initialize + prompt. Process loss
/// (`ChannelClosed`) yields a bounded `ProcessLost` result.
pub fn run_acp_agent(
    channel: &mut dyn AgentChannel,
    task: &ExternalAgentTask,
    cancel: &CancellationToken,
) -> Result<NormalizedAgentResult, ExternalAgentError> {
    let init = acp_initialize_request(1, "rapidlm");
    let response = channel.exchange(&init, cancel)?;
    parse_acp_initialize(&response)?;
    let prompt = acp_prompt_request(2, "main", task)?;
    let bytes = match channel.exchange(&prompt, cancel) {
        Ok(bytes) => bytes,
        // Process loss yields a bounded recovery result, never an error.
        Err(ExternalAgentError::ChannelClosed)
        | Err(ExternalAgentError::Transport(StdioError::Io(_))) => {
            return Ok(normalize_result(
                AgentFlavor::Acp,
                task.session_id(),
                AgentOutcome::ProcessLost,
                "",
            ));
        }
        Err(other) => return Err(other),
    };
    parse_acp_prompt_result(task.session_id(), &bytes)
}

/// Raw CLI exit captured at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliExit {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
}

/// CLI process seam. Fakes under test only; production uses
/// [`SupervisedCliRunner`]. Preparation is separated from execution so a
/// host can approve/issue a broker lease bound to the exact command before
/// spawning.
pub trait CliRunner {
    /// Build the supervised exec spec (bound to this principal/session).
    fn prepare(
        &mut self,
        task: &ExternalAgentTask,
        cancel: &CancellationToken,
    ) -> Result<ExecSpec, ExternalAgentError>;

    /// Execute a prepared spec under a matching lease guard.
    fn run(
        &mut self,
        exec: ExecSpec,
        task: &ExternalAgentTask,
        lease: LeaseUseGuard,
        cancel: &CancellationToken,
    ) -> Result<CliExit, ExternalAgentError>;
}

/// Convenience path: prepare then execute under one call.
pub fn run_cli_agent(
    runner: &mut dyn CliRunner,
    task: &ExternalAgentTask,
    lease: LeaseUseGuard,
    cancel: &CancellationToken,
) -> Result<NormalizedAgentResult, ExternalAgentError> {
    let exec = runner.prepare(task, cancel)?;
    let exit = runner.run(exec, task, lease, cancel)?;
    let outcome = normalize_cli_exit(exit.ok, exit.code);
    Ok(normalize_result(
        AgentFlavor::Cli,
        task.session_id(),
        outcome,
        &exit.stdout,
    ))
}

/// Production CLI transport: supervised child process via the shared
/// process supervisor. Parent env is not copied; stdin carries the prompt.
pub struct SupervisedCliRunner {
    argv: Vec<String>,
    cwd: PathBuf,
    timeout: Duration,
    output_limit: u64,
    principal: PrincipalRef,
}

impl SupervisedCliRunner {
    pub fn new(
        argv: Vec<String>,
        cwd: PathBuf,
        principal: PrincipalRef,
        session_id: SessionId,
        _cancel: CancellationToken,
    ) -> Result<Self, ExternalAgentError> {
        if argv.is_empty() {
            return Err(ExternalAgentError::InvalidTask);
        }
        let _ = session_id;
        Ok(Self {
            argv,
            cwd,
            timeout: DEFAULT_AGENT_TIMEOUT,
            output_limit: MAX_AGENT_RESULT_BYTES as u64,
            principal,
        })
    }
}

impl CliRunner for SupervisedCliRunner {
    fn prepare(
        &mut self,
        task: &ExternalAgentTask,
        cancel: &CancellationToken,
    ) -> Result<ExecSpec, ExternalAgentError> {
        let cwd_text = self.cwd.to_str().ok_or(ExternalAgentError::InvalidTask)?;
        let canonical = CanonicalHostPath::from_resolved(cwd_text)
            .map_err(|_| ExternalAgentError::InvalidTask)?;
        let exec = ExecSpec::argv(
            self.argv.iter().cloned(),
            canonical,
            Vec::<(String, SecretOrValue)>::new(),
            StdinSpec::Bytes(task.prompt().as_bytes().to_vec()),
            Some(self.timeout),
            self.output_limit,
            cancel.clone(),
        )
        .map_err(|_| ExternalAgentError::Supervised("spec".into()))?;
        let binding = ExecBinding::proc_exec(
            self.principal.clone(),
            task.session_id(),
            AGENT_COMMAND_FAMILY,
        )
        .map_err(|_| ExternalAgentError::Supervised("binding".into()))?;
        exec.bind(binding)
            .map_err(|_| ExternalAgentError::Supervised("bind".into()))
    }

    fn run(
        &mut self,
        exec: ExecSpec,
        task: &ExternalAgentTask,
        lease: LeaseUseGuard,
        cancel: &CancellationToken,
    ) -> Result<CliExit, ExternalAgentError> {
        let _ = task;
        let mut handle = spawn(exec, lease)
            .map_err(|_| ExternalAgentError::Supervised("spawn".into()))?;
        let report = await_exit(&mut handle, cancel, DEFAULT_GRACE)
            .map_err(|_| ExternalAgentError::Supervised("await".into()))?;
        // Bounded stdout drain from the child pipe (mirrors hook capture).
        let stdout_bytes = match handle.child_mut().stdout.take() {
            Some(pipe) => {
                use std::io::Read;
                let mut buf = Vec::new();
                let cap = self.output_limit as usize;
                let _ = pipe.take((cap as u64).saturating_add(1)).read_to_end(&mut buf);
                buf.truncate(cap);
                buf
            }
            None => Vec::new(),
        };
        drop(handle);
        let status = report.status();
        let (ok, code) = match status {
            process_supervisor::TerminalStatus::Exited(exit) => match exit.code() {
                Some(0) => (true, Some(0)),
                Some(code) => (false, Some(code)),
                None => (false, None),
            },
            _ => (false, None),
        };
        let text = String::from_utf8_lossy(&stdout_bytes);
        Ok(CliExit {
            ok,
            code,
            stdout: text.into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(prompt: &str) -> ExternalAgentTask {
        ExternalAgentTask::new(SessionId::new(), prompt).expect("task")
    }

    // Deterministic fake at the stdio frame boundary. Test-only by virtue of
    // living inside this test module.
    struct ScriptedChannel {
        responses: Vec<Vec<u8>>,
        closed_after: Option<usize>,
        served: usize,
    }

    impl AgentChannel for ScriptedChannel {
        fn exchange(
            &mut self,
            _request: &JsonRpcMessage,
            _cancel: &CancellationToken,
        ) -> Result<Vec<u8>, ExternalAgentError> {
            if let Some(after) = self.closed_after
                && self.served >= after {
                    return Err(ExternalAgentError::ChannelClosed);
                }
            let response = self
                .responses
                .get(self.served)
                .cloned()
                .ok_or(ExternalAgentError::ChannelClosed)?;
            self.served += 1;
            Ok(response)
        }
    }

    fn result_frame(stop: &str, text: &str) -> Vec<u8> {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "stopReason": stop, "text": text }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn acp_agent_completes_through_initialize_and_prompt() {
        let cancel = CancellationToken::new();
        let mut channel = ScriptedChannel {
            responses: vec![
                serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}})
                    .to_string()
                    .into_bytes(),
                result_frame("end_turn", "patch applied"),
            ],
            closed_after: None,
            served: 0,
        };
        let outcome =
            run_acp_agent(&mut channel, &task("fix the build"), &cancel).expect("run");
        assert_eq!(outcome.flavor(), AgentFlavor::Acp);
        assert_eq!(
            outcome.outcome(),
            &AgentOutcome::Completed {
                stop_reason: "end_turn".to_owned()
            }
        );
        assert_eq!(outcome.text(), "patch applied");
        assert_eq!(outcome.trust_label(), EXTERNAL_TRUST_LABEL);
    }

    #[test]
    fn acp_process_loss_returns_bounded_recovery_result() {
        let cancel = CancellationToken::new();
        let mut channel = ScriptedChannel {
            responses: vec![
                serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}).to_string().into_bytes(),
            ],
            closed_after: Some(1),
            served: 0,
        };
        let outcome =
            run_acp_agent(&mut channel, &task("continue work"), &cancel).expect("bounded run");
        assert_eq!(outcome.outcome(), &AgentOutcome::ProcessLost);
        assert_eq!(outcome.text(), "");
    }

    #[test]
    fn acp_refusal_and_malformed_frames_map_correctly() {
        let session_id = SessionId::new();
        let refused = parse_acp_prompt_result(session_id, &result_frame("refusal", "")).expect("refusal");
        assert_eq!(
            refused.outcome(),
            &AgentOutcome::Failed {
                reason: "refusal".to_owned()
            }
        );
        let malformed = parse_acp_prompt_result(session_id, b"{not json");
        assert!(matches!(malformed, Err(ExternalAgentError::ProtocolFault)));
        let unknown_stop = parse_acp_prompt_result(
            session_id,
            &result_frame("warp_drive", ""),
        );
        assert!(matches!(unknown_stop, Err(ExternalAgentError::ProtocolFault)));
    }

    #[test]
    fn cli_exit_normalization_is_bounded_and_typed() {
        assert_eq!(
            normalize_cli_exit(true, Some(0)),
            AgentOutcome::Completed {
                stop_reason: "exit_zero".to_owned()
            }
        );
        assert_eq!(
            normalize_cli_exit(false, Some(101)),
            AgentOutcome::Failed {
                reason: "exit_101".to_owned()
            }
        );
        assert_eq!(
            normalize_cli_exit(false, None),
            AgentOutcome::Failed {
                reason: "signaled".to_owned()
            }
        );
    }

    #[test]
    fn oversized_external_text_is_truncated_not_propagated_raw() {
        let huge = "x".repeat(MAX_AGENT_RESULT_BYTES * 3);
        let normalized = normalize_result(
            AgentFlavor::Cli,
            SessionId::new(),
            AgentOutcome::Completed {
                stop_reason: "exit_zero".to_owned(),
            },
            &huge,
        );
        assert_eq!(normalized.text().len(), MAX_AGENT_RESULT_BYTES);
    }

    #[test]
    fn capabilities_and_tasks_reject_invalid_declarations() {
        assert!(ExternalAgentCapabilities::new(false, 0, Vec::new()).is_ok());
        let long_tool = "t".repeat(200);
        assert!(ExternalAgentCapabilities::new(false, 4, vec![long_tool]).is_err());
        assert!(ExternalAgentTask::new(SessionId::new(), "").is_err());
        assert!(ExternalAgentTask::new(SessionId::new(), "y".repeat(MAX_AGENT_PROMPT_BYTES + 1)).is_err());
    }

    #[test]
    fn supervised_cli_runner_drives_real_child_through_broker_lease() {
        use capability_broker::{
            ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
            ExecIntent, LeaseIssuer, LeaseValidator, PolicyDocument, PolicyRevision, PolicySource,
            PolicyStack, Resolver, evaluate, issue, normalize_exec, request_approval, validate_use,
        };
        use std::time::Instant;

        struct FrozenPathResolver;

        impl Resolver for FrozenPathResolver {
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
                CanonicalHostPath::from_resolved(requested)
                    .map_err(|_| capability_broker::CommandNormalizeError::UnresolvedExecutable)
            }
        }

        let cancel = CancellationToken::new();
        let principal = PrincipalRef::parse("agent/main").expect("principal");
        let session_id = SessionId::new();

        // Policy: ask (approve once) for proc.exec by this principal.
        let src = r#"
[[rules]]
id = "agent-exec"
effect = "ask"
subjects = ["agent/main"]
capability = "proc.exec"
"#;
        let policies = PolicyStack::new([PolicyDocument::parse_toml(
            src,
            PolicySource::user("agents.toml").expect("source"),
            &cancel,
        )
        .expect("parse")])
        .expect("stack");
        let issuer = LeaseIssuer::from_key([0x5au8; 32]).expect("issuer");
        let validator = LeaseValidator::new(LeaseIssuer::from_key([0x5au8; 32]).expect("issuer2"), PolicyRevision::of_stack(&policies));

        // Production runner drives a real OS child (/bin/cat echoes stdin).
        let tmp = std::env::temp_dir().canonicalize().expect("tmp");
        let mut runner = SupervisedCliRunner::new(
            vec!["/bin/cat".to_owned()],
            tmp,
            principal.clone(),
            session_id,
            cancel.clone(),
        )
        .expect("runner");
        let agent_task = task("normalize me");

        // Prepare first: the lease must be bound to the exact canonical command.
        let spec = runner.prepare(&agent_task, &cancel).expect("prepare");
        let guard = {
            let spec_ref = &spec;
            let binding = spec_ref.binding().expect("bound");
            let env_names = spec_ref.env().keys().cloned();
            let intent = match spec_ref.invocation() {
                process_supervisor::Invocation::Argv { argv } => {
                    ExecIntent::argv(argv.clone(), spec_ref.cwd().as_str().to_owned(), env_names)
                }
                _ => panic!("expected argv invocation"),
            };
            let command =
                normalize_exec(&intent, &FrozenPathResolver, &cancel).expect("canon");
            let action = CanonicalAction::Command(command);
            let request = ActionRequest::new(
                binding.principal().clone(),
                binding.session_id(),
                binding.capability(),
                binding.resource().clone(),
                action.clone(),
                AGENT_COMMAND_FAMILY,
            )
            .expect("request");
            let now = Instant::now();
            let decision = evaluate(&policies, &request, &cancel).expect("evaluate");
            let approval =
                request_approval(&request, &decision, now, &cancel).expect("approval");
            let approved = match approval
                .resolve(ApprovalChoice::Approve(ApprovalScopeId::Once), &request, now, &cancel)
                .expect("resolve")
            {
                ApprovalResolution::Approved(approved) => approved,
                ApprovalResolution::Denied => panic!("expected approval"),
            };
            let lease = issue(&issuer, &approved, &policies, now, &cancel).expect("issue");
            validate_use(&validator, &lease, &action, now, &cancel).expect("guard")
        };

        let exit = runner
            .run(spec, &agent_task, guard, &cancel)
            .expect("supervised run");
        assert!(exit.ok);
        assert_eq!(exit.stdout.trim_end(), "normalize me");

        // Second turn through the normalization entry point.
        let task2 = task("second pass");
        let spec2 = runner.prepare(&task2, &cancel).expect("prepare2");
        let guard2 = {
            let binding = spec2.binding().expect("bound2");
            let env_names = spec2.env().keys().cloned();
            let intent = match spec2.invocation() {
                process_supervisor::Invocation::Argv { argv } => {
                    ExecIntent::argv(argv.clone(), spec2.cwd().as_str().to_owned(), env_names)
                }
                _ => panic!("expected argv invocation"),
            };
            let command =
                normalize_exec(&intent, &FrozenPathResolver, &cancel).expect("canon2");
            let action = CanonicalAction::Command(command);
            let request = ActionRequest::new(
                binding.principal().clone(),
                binding.session_id(),
                binding.capability(),
                binding.resource().clone(),
                action.clone(),
                AGENT_COMMAND_FAMILY,
            )
            .expect("request2");
            let now2 = Instant::now();
            let decision2 = evaluate(&policies, &request, &cancel).expect("evaluate2");
            let approval2 =
                request_approval(&request, &decision2, now2, &cancel).expect("approval2");
            let approved2 = match approval2
                .resolve(ApprovalChoice::Approve(ApprovalScopeId::Once), &request, now2, &cancel)
                .expect("resolve2")
            {
                ApprovalResolution::Approved(a) => a,
                ApprovalResolution::Denied => panic!("expected approval 2"),
            };
            let lease2 = issue(&issuer, &approved2, &policies, now2, &cancel).expect("issue2");
            validate_use(&validator, &lease2, &action, now2, &cancel).expect("guard2")
        };

        let result = run_cli_agent(&mut runner, &task2, guard2, &CancellationToken::new())
            .expect("normalized");
        assert_eq!(
            result.outcome(),
            &AgentOutcome::Completed {
                stop_reason: "exit_zero".to_owned()
            }
        );
        assert_eq!(result.trust_label(), EXTERNAL_TRUST_LABEL);
        assert_eq!(result.text().trim_end(), "second pass");
    }
}
