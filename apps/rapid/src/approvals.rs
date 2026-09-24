//! Durable pending approvals and clarifications — the bridge between a tool
//! call the permission lattice marked `Ask` and the human who decides it.
//!
//! The flow this module serves:
//!
//! 1. A tool call reaches [`crate::permissions::Decision::Ask`]. Where no
//!    approval surface exists (headless exec without a resolver), the call is
//!    denied exactly as before — fail-closed. Where a surface exists (the
//!    interactive TUI, an ACP/SDK client through the daemon), the driver
//!    hands an [`ApprovalRequest`] to an [`ApprovalSink`] and returns
//!    `ToolStepResult::ApprovalRequired`, which stops the turn without
//!    executing the call.
//! 2. The host persists the turn's exact mid-state (the [`SuspendedTurn`],
//!    every completed exchange plus the pending placeholder) as the
//!    `approval.requested` payload's `detail` and finishes the turn as
//!    [`kernel::TurnOutcome::Waiting`] — releasing the execution lease while
//!    the human decides.
//! 3. The surface renders the pending call (action, scope, diff) and
//!    resolves it with [`kernel::ResolveApproval`] — approve once, approve
//!    and remember (a scoped persisted grant), or deny. Resolution is
//!    durable: a restarted process re-derives the pending set from the
//!    ledger and offers the same choices.
//! 4. On approval the host rebuilds the turn from the suspension, executes
//!    the pending call exactly once, and continues the model loop from the
//!    recorded history — no committed side effect is repeated, and a denial
//!    is fed back to the model as a typed `Denied` result.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use event_ledger::event::ActorRef;
use kernel::{ApprovalRequestedPayload, InProcessKernelClient, KernelClient, PendingApproval};

use crate::line_diff;

/// The driver-facing description of one call that needs approval: what the
/// surface must show a human, plus nothing else. `arguments` is the raw JSON
/// the model proposed (bounded by the call validator upstream).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub call_id: String,
    pub summary: String,
    pub scope: Vec<String>,
    pub diff: String,
    /// Who raised the ask when it was not the permission lattice — a hook
    /// (`hook:pre_tool_use[0]#<command digest>`), later a plan or an
    /// elicitation (ADR 0022 §3). `None` for a lattice `Ask`.
    pub source: Option<String>,
    /// SHA-256 of the arguments this request describes — what the human is
    /// approving. The resume runs a call only when its arguments match.
    pub arguments_digest: Option<String>,
    /// The persisted grant that would answer the same call from then on,
    /// set only by a lattice `Ask` that has one
    /// (`PermissionLattice::standing_grant_for`); what `rapid acp`'s "Allow
    /// always" records, and offered only with it. `None` for every other
    /// ask — a hook's, a question, a shell command, a call a rule or policy
    /// ranked above grants still decides.
    pub remember_as: Option<String>,
}

/// Lowercase hex SHA-256 of `arguments` — the digest approvals and hook
/// records use for tool arguments.
pub fn arguments_digest(arguments: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(arguments.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the human approved, read back from the resolved `approval.requested`
/// record for the resume: who asked and the digest of the arguments shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovedAsk {
    pub source: Option<String>,
    pub arguments_digest: Option<String>,
}

impl ApprovedAsk {
    pub fn from_payload(payload: &ApprovalRequestedPayload) -> Self {
        Self {
            source: payload.source.clone(),
            arguments_digest: payload.arguments_digest.clone(),
        }
    }

    /// Whether `arguments` are the arguments the human was shown. A record
    /// without a digest (written before the field existed) matches nothing —
    /// fail closed: the call is asked about again rather than assumed.
    pub fn covers(&self, arguments: &str) -> bool {
        self.arguments_digest.as_deref() == Some(arguments_digest(arguments).as_str())
    }
}

/// Records a pending approval durably. Returns the wait token the request was
/// recorded under, or the reason the request could not be recorded — a
/// caller treats any error as "no approval surface available" and denies
/// fail-closed, exactly as if no sink were configured.
pub trait ApprovalSink: Send + Sync {
    fn request(&self, request: &ApprovalRequest) -> Result<String, String>;
}

/// The suspended mid-turn state a resume replays: the original task text and
/// the turn loop's own record of every completed exchange, ending with the
/// batch holding the pending call's placeholder result. Serialized into the
/// `tool.approval_required` progress event at the session tip; loaded again
/// by this process, a restarted TUI, or an SDK client's resume path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspendedTurn {
    pub task: String,
    pub call_id: String,
    pub reason: String,
    pub omitted_earlier_steps: u32,
    pub history: SuspendedHistory,
}

/// Wire form of [`agent_runtime::ToolStepExchange`] — a local mirror because
/// the app serializes the suspension into a ledger payload and the
/// agent-runtime types own their own serde forms. Kept in one place
/// (`from_agent`/`to_agent`) rather than leaking serde onto the loop's types
/// at every call site.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspendedHistory {
    pub exchanges: Vec<SuspendedExchange>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspendedExchange {
    pub calls: Vec<SuspendedCall>,
    pub results: Vec<SuspendedResult>,
}

/// Rebuild one recorded call. A recorded call passed validation when the
/// original turn accepted it, so the replay constructor (not the validating
/// one) is used: bounds may have tightened between processes, and refusing to
/// replay a lawfully-accepted call would strand the suspension.
fn suspended_call_to_agent(call: &SuspendedCall) -> agent_runtime::ProposedToolCall {
    agent_runtime::ProposedToolCall::replay(
        call.call_id.clone(),
        call.tool.clone(),
        call.arguments.clone(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspendedCall {
    pub call_id: String,
    pub tool: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspendedResult {
    pub kind: String,
    pub call_id: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub handled: bool,
    #[serde(default)]
    pub question: String,
}

impl SuspendedHistory {
    pub fn from_agent(history: &[agent_runtime::ToolStepExchange]) -> Self {
        Self {
            exchanges: history
                .iter()
                .map(|exchange| SuspendedExchange {
                    calls: exchange
                        .calls()
                        .iter()
                        .map(|call| SuspendedCall {
                            call_id: call.call_id().to_owned(),
                            tool: call.tool().to_owned(),
                            arguments: call.arguments().to_owned(),
                        })
                        .collect(),
                    results: exchange
                        .results()
                        .iter()
                        .map(|result| SuspendedResult {
                            kind: result_kind(result).to_owned(),
                            call_id: result_call_id(result).to_owned(),
                            summary: match result {
                                agent_runtime::ToolStepResult::Succeeded { summary, .. } => {
                                    summary.clone()
                                }
                                _ => String::new(),
                            },
                            detail: match result {
                                agent_runtime::ToolStepResult::Failed { detail, .. }
                                | agent_runtime::ToolStepResult::Denied { detail, .. } => {
                                    detail.clone()
                                }
                                _ => None,
                            },
                            handled: matches!(
                                result,
                                agent_runtime::ToolStepResult::Failed { handled: true, .. }
                            ),
                            question: match result {
                                agent_runtime::ToolStepResult::ContextRequired {
                                    question, ..
                                } => question.clone(),
                                _ => String::new(),
                            },
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Back into the loop's own types. The `from_agent` inverse in structure;
    /// an unknown result `kind` degrades to `Failed` rather than being
    /// dropped, so the model always sees one result per call.
    pub fn to_agent(&self) -> Vec<agent_runtime::ToolStepExchange> {
        self.exchanges
            .iter()
            .map(|exchange| {
                agent_runtime::ToolStepExchange::new(
                    exchange.calls.iter().map(suspended_call_to_agent).collect(),
                    exchange
                        .results
                        .iter()
                        .map(|result| match result.kind.as_str() {
                            "succeeded" => agent_runtime::ToolStepResult::Succeeded {
                                call_id: result.call_id.clone(),
                                summary: result.summary.clone(),
                            },
                            "denied" => agent_runtime::ToolStepResult::Denied {
                                call_id: result.call_id.clone(),
                                detail: result.detail.clone(),
                            },
                            "context_required" => agent_runtime::ToolStepResult::ContextRequired {
                                call_id: result.call_id.clone(),
                                question: result.question.clone(),
                            },
                            _ => agent_runtime::ToolStepResult::Failed {
                                call_id: result.call_id.clone(),
                                handled: result.handled,
                                detail: result.detail.clone(),
                            },
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// The exchange holding `call_id`, and its index. The resume path
    /// replaces that result (placeholder → real outcome) in place.
    pub fn find_result_mut(&mut self, call_id: &str) -> Option<(&mut SuspendedExchange, usize)> {
        let index = self.exchanges.iter().position(|exchange| {
            exchange
                .results
                .iter()
                .any(|result| result.call_id == call_id)
        })?;
        Some((&mut self.exchanges[index], index))
    }

    pub fn result_of(&self, call_id: &str) -> Option<&SuspendedResult> {
        for exchange in &self.exchanges {
            for result in &exchange.results {
                if result.call_id == call_id {
                    return Some(result);
                }
            }
        }
        None
    }
}

fn result_kind(result: &agent_runtime::ToolStepResult) -> &'static str {
    match result {
        agent_runtime::ToolStepResult::Succeeded { .. } => "succeeded",
        agent_runtime::ToolStepResult::Failed { .. } => "failed",
        agent_runtime::ToolStepResult::Denied { .. } => "denied",
        agent_runtime::ToolStepResult::ApprovalRequired { .. } => "approval_required",
        agent_runtime::ToolStepResult::ContextRequired { .. } => "context_required",
    }
}

fn result_call_id(result: &agent_runtime::ToolStepResult) -> &str {
    match result {
        agent_runtime::ToolStepResult::Succeeded { call_id, .. }
        | agent_runtime::ToolStepResult::Failed { call_id, .. }
        | agent_runtime::ToolStepResult::Denied { call_id, .. }
        | agent_runtime::ToolStepResult::ApprovalRequired { call_id }
        | agent_runtime::ToolStepResult::ContextRequired { call_id, .. } => call_id,
    }
}

/// Mint a wait token. Unique per process by counter + boot nanos; the wait
/// table's own uniqueness constraint is the durable backstop.
pub fn new_wait_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Hex and hyphens only: the TUI's approval projection keys on this id
    // and its parser admits exactly that alphabet.
    format!("{nanos:x}-{count:x}")
}

/// Extract `(summary, scope, diff)` for a pending call from its tool name and
/// JSON arguments. Everything is best-effort: a malformed argument payload
/// still produces a summary (the tool name plus the bounded raw arguments) so
/// the human always sees *something* true about what was asked.
pub fn describe_call(tool: &str, arguments: &str, root: &Path) -> (String, Vec<String>, String) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(arguments).ok();
    let arg = |key: &str| -> Option<String> {
        parsed
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    match tool {
        "workspace_write" => {
            let Some(path) = arg("path") else {
                return fallback(tool, arguments);
            };
            let absolute = root.join(&path);
            let before = std::fs::read_to_string(&absolute).unwrap_or_default();
            let after = arg("content").unwrap_or_default();
            let diff = match line_diff::unified(&before, &after) {
                line_diff::DiffOutcome::Unified(text) => text,
                line_diff::DiffOutcome::Identical => String::new(),
                line_diff::DiffOutcome::TooLarge => {
                    "(file too large to diff; review with /diff after approval)".to_owned()
                }
            };
            let summary = if before.is_empty() {
                format!("create {path}")
            } else {
                format!("overwrite {path}")
            };
            (summary, vec![path], diff)
        }
        "workspace_patch" => {
            let path = arg("path").unwrap_or_else(|| "(patch)".to_owned());
            let summary = format!("patch {path}");
            let diff = parsed
                .as_ref()
                .and_then(|value| value.get("patch"))
                .and_then(|value| value.as_str())
                .map(|patch| bounded_diff(patch.to_owned()))
                .unwrap_or_default();
            (summary, vec![path], diff)
        }
        "shell_exec" => {
            // The tool's arguments are `argv` (a JSON array); the summary
            // used to read a `command` string the tool never had, so every
            // shell approval said "run: (no command)" — a human approving a
            // command they could not see. The joined argv is the command.
            let command = parsed
                .as_ref()
                .and_then(|value| value.get("argv"))
                .and_then(|value| value.as_array())
                .map(|argv| {
                    argv.iter()
                        .map(|item| match item {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|command| !command.is_empty())
                .or_else(|| arg("command"))
                .unwrap_or_else(|| "(no command)".to_owned());
            let cwd = arg("cwd").unwrap_or_else(|| ".".to_owned());
            (format!("run: {command}"), vec![cwd], String::new())
        }
        "web_fetch" => {
            let url = arg("url").unwrap_or_else(|| "(no url)".to_owned());
            (format!("fetch {url}"), vec![url], String::new())
        }
        "workspace_read" | "repo_read" => {
            let path = arg("path").unwrap_or_else(|| "(path)".to_owned());
            (format!("read {path}"), vec![path], String::new())
        }
        "repo_search" => {
            let pattern = arg("pattern").unwrap_or_else(|| "(pattern)".to_owned());
            (format!("search for {pattern}"), Vec::new(), String::new())
        }
        "repo_glob" => {
            let pattern = arg("pattern").unwrap_or_else(|| "(pattern)".to_owned());
            (format!("glob {pattern}"), Vec::new(), String::new())
        }
        "task_spawn" => {
            let prompt = arg("prompt").unwrap_or_default();
            let mut excerpt: String = prompt.chars().take(160).collect();
            if prompt.chars().count() > 160 {
                excerpt.push('…');
            }
            (
                format!("delegate a subagent: {excerpt}"),
                Vec::new(),
                String::new(),
            )
        }
        _ => fallback(tool, arguments),
    }
}

fn fallback(tool: &str, arguments: &str) -> (String, Vec<String>, String) {
    let mut bounded: String = arguments.chars().take(200).collect();
    if arguments.chars().count() > 200 {
        bounded.push('…');
    }
    (format!("{tool}: {bounded}"), Vec::new(), String::new())
}

fn bounded_diff(diff: String) -> String {
    if diff.len() <= line_diff::MAX_UNIFIED_BYTES {
        return diff;
    }
    let mut end = line_diff::MAX_UNIFIED_BYTES;
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &diff[..end])
}

/// Build the full [`ApprovalRequest`] for a pending call, including the diff
/// for file writes. `root` is the workspace the call would act on.
pub fn build_request(tool: &str, call_id: &str, arguments: &str, root: &Path) -> ApprovalRequest {
    let (summary, scope, diff) = describe_call(tool, arguments, root);
    ApprovalRequest {
        tool: tool.to_owned(),
        call_id: call_id.to_owned(),
        summary,
        scope,
        diff,
        source: None,
        arguments_digest: Some(arguments_digest(arguments)),
        remember_as: None,
    }
}

/// Bound on the hook reason carried into an approval summary: the summary
/// itself is bounded at [`kernel::MAX_APPROVAL_SUMMARY_BYTES`] (512) and a
/// hook reason may be 2 KiB, so the reason is cut first — the call's own
/// description (for `shell_exec`, the only place the command appears) must
/// never be what the bound removes.
pub const MAX_HOOK_ASK_REASON_BYTES: usize = 192;

/// The [`ApprovalRequest`] a hook's `ask` raises for a call: the same
/// action, scope and diff a lattice `Ask` would show, then the hook and its
/// (bounded) reason, with the hook as the request's `source`. The call comes
/// first because a human approving a command must see the command — a long
/// reason is cut, the action never is.
pub fn build_hook_ask_request(
    tool: &str,
    call_id: &str,
    arguments: &str,
    root: &Path,
    hook: &str,
    command_digest: &str,
    reason: &str,
) -> ApprovalRequest {
    let mut request = build_request(tool, call_id, arguments, root);
    let mut reason = reason.trim().to_owned();
    if reason.len() > MAX_HOOK_ASK_REASON_BYTES {
        let mut end = MAX_HOOK_ASK_REASON_BYTES;
        while end > 0 && !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push('…');
    }
    request.summary = format!("{} — {hook} hook asks: {reason}", request.summary);
    request.source = Some(hook_source(hook, command_digest));
    request
}

/// The `source` a hook's ask carries: the hook's position *and* the digest
/// of its command line, so an approval answers the hook that asked — not
/// whichever hook sits at that position after the settings file changed.
pub fn hook_source(hook: &str, command_digest: &str) -> String {
    format!("hook:{hook}#{command_digest}")
}

/// The sink the interactive surface installs on `ExecTools`: every pending
/// approval is journaled (`approval.requested` + a durable wait row) before
/// the call may run, so a crash or a restart never loses the question.
#[derive(Clone)]
pub struct LedgerApprovalSink {
    client: InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: ActorRef,
    /// Retained as the sink's workspace identity for diagnostics and future
    /// scoping; the journal itself keys off the ledger client.
    #[allow(dead_code)]
    root: PathBuf,
    /// One request at a time through this sink: a tool batch runs its calls
    /// on threads, and two asks reading the same session tip would race on
    /// `expected_seq` — the loser was told there was no approval surface.
    /// Serialising the sink's own requests removes the common race; the
    /// bounded retry below covers another writer's append landing between
    /// the tip read and ours.
    requests: std::sync::Arc<std::sync::Mutex<()>>,
}

impl LedgerApprovalSink {
    pub fn new(
        client: InProcessKernelClient,
        session_id: protocol::SessionId,
        actor: ActorRef,
        root: PathBuf,
    ) -> Self {
        Self {
            client,
            session_id,
            actor,
            root,
            requests: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    /// The session tip the `approval.requested` must append at — read fresh
    /// per request, because a turn's tool batch runs concurrently with its
    /// own progress appends.
    fn tip(&self) -> Result<u64, String> {
        let snapshot = client_call(self.client.get_session(self.session_id))?;
        Ok(snapshot.seq())
    }
}

fn with_source(
    record: kernel::RecordApproval,
    request: &ApprovalRequest,
) -> kernel::RecordApproval {
    let record = match request.source.as_deref() {
        Some(source) => record.with_source(source),
        None => record,
    };
    let record = match request.remember_as.as_deref() {
        Some(pattern) => record.with_remember_as(pattern),
        None => record,
    };
    match request.arguments_digest.as_deref() {
        Some(digest) => record.with_arguments_digest(digest),
        None => record,
    }
}

/// How many times a request re-reads the session tip after another writer
/// advanced it. A tool batch runs its calls on threads, so two hook asks
/// (or an ask beside a job's progress event) can both read the same tip;
/// the loser must not be told there is no approval surface. Generous: a
/// writer can lose every race to a neighbour that keeps appending (a second
/// sink's whole batch, a job spooling output), and each retry is one cheap
/// local read.
const REQUEST_SEQUENCE_RETRIES: usize = 64;

impl ApprovalSink for LedgerApprovalSink {
    fn request(&self, request: &ApprovalRequest) -> Result<String, String> {
        let _one_at_a_time = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut attempt = 0;
        loop {
            // A fresh token per attempt: the kernel releases a failed
            // attempt's wait row as `Expired` but keeps it, so the same token
            // would collide on the retry.
            let token = new_wait_token();
            let expected_seq = self.tip()?;
            let outcome = poll_once(
                self.client.record_approval(with_source(
                    kernel::RecordApproval::new(
                        self.session_id,
                        expected_seq,
                        self.actor.clone(),
                        protocol::TraceId::new(),
                        token.clone(),
                        request.call_id.clone(),
                        request.tool.clone(),
                        request.summary.clone(),
                    )
                    .with_scope(request.scope.clone())
                    .with_diff(request.diff.clone()),
                    request,
                )),
            )?;
            match outcome {
                Ok(()) => return Ok(token),
                Err(err)
                    if err.code() == protocol::ErrorCode::SessionConflict
                        && attempt < REQUEST_SEQUENCE_RETRIES =>
                {
                    attempt += 1;
                }
                Err(err) => return Err(err.to_string()),
            }
        }
    }
}

/// Poll one kernel call once, keeping its typed error (unlike
/// [`client_call`], which stringifies it) so the caller can tell a sequence
/// conflict — retryable — from every other failure.
fn poll_once<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<Result<T, E>, String> {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => Ok(result),
        std::task::Poll::Pending => Err("kernel did not answer immediately".to_owned()),
    }
}

/// Poll one kernel call to completion. Kernel calls here are local SQLite
/// appends/reads — always immediately ready on a single poll, exactly the
/// assumption `interactive.rs`'s own `block_on` documents; `Pending` is
/// treated as an error so a surprise never silently stalls a tool batch.
pub fn client_call<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<T, String>
where
    E: std::fmt::Display,
{
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => result.map_err(|err| err.to_string()),
        std::task::Poll::Pending => Err("kernel did not answer immediately".to_owned()),
    }
}

use std::future::Future;

/// Persist a turn's suspension: the exact mid-state a resume replays,
/// recorded as the `tool.approval_required` progress event carrying the
/// pending call's `approval_token`. Best-effort at the caller's discretion —
/// a failure leaves the approval still pending but unresumable in this
/// process, which the caller surfaces as an error line.
pub fn record_suspension(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    actor: &ActorRef,
    token: &str,
    call_id: &str,
    tool: &str,
    suspended: &SuspendedTurn,
) -> Result<(), String> {
    let detail = serde_json::to_string(suspended)
        .map_err(|err| format!("suspension could not be encoded: {err}"))?;
    let payload = serde_json::json!({
        // `id` is what the TUI's approval projection keys on; `token` names
        // the same wait token for the resume path.
        "id": token,
        "call_id": call_id,
        "tool": tool,
        "request_id": null,
        "step": null,
        "tokens": null,
        "detail": null,
        "approval_token": token,
        "suspension": detail,
    });
    client
        .append_turn_progress(
            session_id,
            actor,
            protocol::TraceId::new(),
            event_ledger::event::EventKind::ToolApprovalRequired,
            payload,
        )
        .map_err(|err| err.to_string())
}

/// Load the suspension recorded for `token`, if the process that requested
/// the approval recorded one.
pub fn recorded_suspension(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    token: &str,
) -> Option<SuspendedTurn> {
    let detail = client_call(client.approval_detail(session_id, token))
        .ok()
        .flatten()?;
    serde_json::from_str(&detail).ok()
}

/// The wait token of the newest unresolved `approval.requested` recorded
/// for `call_id` — the linkage between a turn's suspension (which knows the
/// call) and the durable wait row (which knows the token). Newest, because
/// call ids repeat across turns: an older approval for the same id (left
/// pending when its prompt was cancelled) belongs to another turn, and a
/// suspension recorded under its token would resume the wrong wait.
pub fn pending_token_for_call(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    call_id: &str,
) -> Option<String> {
    pending_approvals(client, session_id)
        .into_iter()
        .rev()
        .find(|pending| pending.payload().call_id == call_id)
        .map(|pending| pending.payload().id.clone())
}

/// Poll one kernel approval resolution to completion (see `client_call`).
pub fn client_approve(
    client: &InProcessKernelClient,
    request: kernel::ResolveApproval,
) -> Result<(), String> {
    client_call(client.approve(request))
}

/// Poll one turn submission to completion (see `client_call`).
pub fn client_submit_turn(
    client: &InProcessKernelClient,
    request: kernel::SubmitTurn,
) -> Result<kernel::TurnHandle, String> {
    client_call(client.submit_turn(request))
}

/// Every unresolved `approval.requested` for a session, oldest first.
pub fn pending_approvals(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
) -> Vec<PendingApproval> {
    client_call(client.pending_approvals(session_id)).unwrap_or_default()
}

/// The payload of one `approval.requested`, by token — the suspended-turn
/// detail lives in the sibling `tool.approval_required` progress event, so
/// the resume path reads both.
pub fn requested_payload(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    token: &str,
) -> Option<ApprovalRequestedPayload> {
    pending_approvals(client, session_id)
        .into_iter()
        .find(|pending| pending.payload().id == token)
        .map(|pending| pending.payload().clone())
}

/// The `approval.requested` payload recorded under `token`, pending or
/// already resolved — what a continuation needs after the human decided
/// (the pending set no longer holds it): who raised the ask (`source`) and
/// about what.
pub fn recorded_request(
    client: &InProcessKernelClient,
    session_id: protocol::SessionId,
    token: &str,
) -> Option<ApprovalRequestedPayload> {
    let events = client
        .export_events(session_id, &kernel::CancellationToken::new())
        .ok()?;
    events
        .iter()
        .filter(|event| event.kind == event_ledger::event::EventKind::ApprovalRequested.as_str())
        .filter_map(|event| {
            serde_json::from_str::<ApprovalRequestedPayload>(&event.payload_json).ok()
        })
        .find(|payload| payload.id == token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_id_s_token_is_its_newest_pending_approval() {
        let path = std::env::temp_dir().join(format!(
            "rapidlm-approvals-newest-{}-{}.sqlite",
            std::process::id(),
            new_wait_token()
        ));
        let client = InProcessKernelClient::open(&path).expect("ledger");
        let actor = ActorRef::new(
            event_ledger::event::ActorKind::Human,
            &protocol::EventId::new().to_string(),
        )
        .expect("actor");
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .expect("session")
        .id();
        // Two turns proposed a call with the same id; the first one's
        // approval was left pending (its prompt was cancelled).
        for token in ["token-older", "token-newer"] {
            let tip = client_call(client.get_session(session)).expect("tip").seq();
            client_call(client.record_approval(kernel::RecordApproval::new(
                session,
                tip,
                actor.clone(),
                protocol::TraceId::new(),
                token,
                "call_1",
                "workspace_write",
                "write a file",
            )))
            .expect("record");
        }
        assert_eq!(
            pending_token_for_call(&client, session, "call_1").as_deref(),
            Some("token-newer")
        );
        assert_eq!(pending_token_for_call(&client, session, "call_2"), None);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut file = path.clone().into_os_string();
            file.push(suffix);
            let _ = std::fs::remove_file(file);
        }
    }

    #[test]
    fn a_shell_approval_summary_shows_the_argv_command() {
        // The tool's arguments are `argv`; the summary must show them (it
        // used to read a `command` field the tool never sends).
        let root = std::env::temp_dir();
        let (summary, scope, diff) = describe_call(
            "shell_exec",
            r#"{"argv":["git","status","--short"],"cwd":"."}"#,
            &root,
        );
        assert_eq!(summary, "run: git status --short");
        assert_eq!(scope, vec![".".to_owned()]);
        assert!(diff.is_empty());
        let (summary, _, _) = describe_call("shell_exec", r#"{"argv":[]}"#, &root);
        assert_eq!(summary, "run: (no command)");
    }

    #[test]
    fn a_hook_ask_request_shows_the_call_first_and_bounds_the_reason() {
        // The summary is bounded at 512 bytes by the kernel; a hook reason
        // may be 2 KiB. The call's own description — for shell_exec the only
        // place the command appears — comes first and is never what the
        // bound removes; the reason is cut with an ellipsis.
        let root = std::env::temp_dir();
        let long = "why ".repeat(300);
        let request = build_hook_ask_request(
            "shell_exec",
            "c1",
            r#"{"argv":["rm","-rf","build"]}"#,
            &root,
            "pre_tool_use[0]",
            "0123456789ab",
            &long,
        );
        assert_eq!(
            request.source.as_deref(),
            Some("hook:pre_tool_use[0]#0123456789ab")
        );
        assert_eq!(
            request.arguments_digest.as_deref(),
            Some(arguments_digest(r#"{"argv":["rm","-rf","build"]}"#).as_str())
        );
        assert!(
            request.summary.starts_with("run: rm -rf build"),
            "{}",
            request.summary
        );
        let asks = request
            .summary
            .find(" — pre_tool_use[0] hook asks: ")
            .expect("the hook follows the call");
        let reason = &request.summary[asks + " — pre_tool_use[0] hook asks: ".len()..];
        assert!(reason.ends_with('…'), "{reason}");
        assert!(reason.len() <= MAX_HOOK_ASK_REASON_BYTES + '…'.len_utf8());
        assert!(
            request.summary.len() <= kernel::MAX_APPROVAL_SUMMARY_BYTES,
            "{} bytes",
            request.summary.len()
        );
        // A short reason is carried whole.
        let short = build_hook_ask_request(
            "repo_read",
            "c2",
            r#"{"path":"a.txt"}"#,
            &root,
            "pre_tool_use[1]",
            "abcdef012345",
            "look",
        );
        assert!(
            short
                .summary
                .ends_with(" — pre_tool_use[1] hook asks: look"),
            "{}",
            short.summary
        );
    }
}
