//! `rapid acp` — serve RapidLM as an ACP agent over stdio.
//!
//! The protocol adapters in `crates/acp` are complete; what was missing was
//! the serve loop that binds them to a real project workspace and actually
//! *executes* submitted turns. This module is that loop:
//!
//! - `initialize` / `session/new` / `session/load` / `session/cancel` are
//!   answered by the adapter directly (it drives the kernel client).
//! - `session/prompt` submits the turn through the adapter, then a turn
//!   thread executes it with the production assembly (hooks, MCP, retrieval,
//!   the durable approval sink) while the loop streams kernel events back as
//!   `session/update` notifications and ends the prompt with the mapped
//!   stop reason.
//! - A pending approval surfaces as a real `session/request_permission`
//!   request to the editor; the editor's decision lands through the same
//!   durable approval machinery the TUI uses (`approvals.rs`), and the
//!   paused turn is resumed as a continuation — the exact pending action
//!   executes once, never the completed side effects.
//!
//! Frames are newline-delimited JSON-RPC over stdio (`crates/acp::stdio`).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use acp::stdio::{FrameReader, FrameWriter, JsonRpcId, JsonRpcMessage, MAX_FRAME_BYTES};
use acp::v1::{MappedEvent, V1Adapter, map_kernel_event};
use kernel::InProcessKernelClient;
use protocol::ProjectId;

use crate::interactive::{acp_resolve_and_continue, spawn_acp_turn};

/// One editor response routed to the waiting prompt thread.
type PendingPermit = Sender<serde_json::Value>;

pub const ACP_USAGE: &str = "\
usage: rapid acp

Serve RapidLM as an ACP (Agent Client Protocol) agent over stdio, for
editors and other ACP clients (Zed and anything else speaking ACP v1/v2).

The project workspace is resolved from the current directory the same way
`rapid exec` resolves it: trust gates tool access, the permission lattice
gates every tool call, and a pending approval surfaces to the editor as a
`session/request_permission` request whose decision resumes the exact turn.

Exit codes: 0 clean disconnect · 2 usage · 1 protocol failure.
";

/// The ACP serve's mode control: the six permission modes the runtime
/// genuinely implements, backed by the shared override cell every turn's
/// lattice resolution reads. This is what makes the `session/set_mode`
/// advertisement truthful — the switch changes real permission behavior on
/// the next prompt, ahead of env/settings, still narrowed by the
/// managed-policy ceiling.
#[derive(Clone)]
struct RapidSessionModes {
    override_cell: Arc<Mutex<Option<crate::permissions::PermissionMode>>>,
}

impl acp::v1::SessionModeControl for RapidSessionModes {
    fn modes(&self) -> Vec<(String, String)> {
        [
            ("default", "Default"),
            ("plan", "Plan"),
            ("acceptEdits", "Accept edits"),
            ("auto", "Auto"),
            ("dontAsk", "Don't ask"),
            ("bypassPermissions", "Bypass permissions"),
        ]
        .into_iter()
        .map(|(id, name)| (id.to_owned(), name.to_owned()))
        .collect()
    }

    fn current_mode(&self) -> String {
        self.override_cell
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|mode| crate::permissions::MODE_NAMES[*mode as usize].to_owned())
            .unwrap_or_else(|| "default".to_owned())
    }

    fn set_mode(&self, id: &str) -> Result<(), String> {
        let mode = crate::permissions::PermissionMode::parse(id)
            .ok_or_else(|| format!("unknown mode {id}"))?;
        *self.override_cell.lock().unwrap_or_else(|p| p.into_inner()) = Some(mode);
        Ok(())
    }
}

/// `rapid acp`: bind the ACP adapters to this workspace over stdio.
pub fn run_acp(args: &[String]) -> Result<i32, crate::p9_commands::P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{ACP_USAGE}");
        return Ok(0);
    }
    if !args.is_empty() {
        return Err(crate::p9_commands::P9CommandError::Usage);
    }
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid acp: no project workspace resolved");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };
    if !trusted {
        eprintln!(
            "warning: the project is not trusted; the agent will refuse every tool call. \
Approve trust with `rapid trust grant`."
        );
    }
    let ledger_path =
        crate::interactive::project_ledger_path(&root.join(crate::interactive::PROJECT_MARKER));
    if let Some(parent) = ledger_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let client = InProcessKernelClient::open(&ledger_path)
        .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;
    let actor = event_ledger::event::ActorRef::new(
        event_ledger::event::ActorKind::Agent,
        &protocol::EventId::new().to_string(),
    )
    .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;

    let serve_cancel = acp::stdio::CancellationToken::new();
    let mode_override: Arc<Mutex<Option<crate::permissions::PermissionMode>>> =
        Arc::new(Mutex::new(None));
    let adapter = V1Adapter::new(
        client.clone(),
        ProjectId::new(),
        actor.clone(),
        serve_cancel.clone(),
    )
    .with_session_modes(Arc::new(RapidSessionModes {
        override_cell: mode_override.clone(),
    }));
    let serve = Serve {
        client,
        adapter: std::cell::RefCell::new(adapter),
        actor,
        root,
        trusted,
        pending: Arc::new(Mutex::new(None)),
        mode_override,
    };
    match serve.run(std::io::stdin(), std::io::stdout(), serve_cancel) {
        Ok(()) => Ok(0),
        Err(reason) => {
            eprintln!("rapid acp: {reason}");
            Ok(1)
        }
    }
}

struct Serve {
    client: InProcessKernelClient,
    /// The adapter is driven sequentially by the loop; interior mutability
    /// is its own (cursors, initialized). `RefCell` is sound here: one
    /// thread drives `handle`, and the turn thread never touches it.
    adapter: std::cell::RefCell<V1Adapter<InProcessKernelClient>>,
    actor: event_ledger::event::ActorRef,
    root: PathBuf,
    trusted: bool,
    /// Where the live prompt thread waits for the editor's permission
    /// decision. `Some` only while a prompt is in flight.
    pending: Arc<Mutex<Option<PendingPermit>>>,
    /// The session's permission-mode cell: written by
    /// `session/set_mode` (through [`RapidSessionModes`]) and read by
    /// every spawned turn's lattice resolution.
    mode_override: Arc<Mutex<Option<crate::permissions::PermissionMode>>>,
}

impl Serve {
    fn run<R: std::io::Read + Send + 'static, W: std::io::Write + Send + 'static>(
        mut self,
        stdin: R,
        stdout: W,
        cancel: acp::stdio::CancellationToken,
    ) -> Result<(), String> {
        let mut reader = FrameReader::new(stdin, MAX_FRAME_BYTES).map_err(|err| err.to_string())?;
        let (out_tx, out_rx) = std::sync::mpsc::channel::<JsonRpcMessage>();
        let (in_tx, in_rx) = std::sync::mpsc::channel::<JsonRpcMessage>();
        // The reader runs on its own thread: stdin blocks, and the loop must
        // keep consuming (cancel notifications, permission decisions) while
        // a turn streams.
        {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                loop {
                    match reader.read_frame(&cancel) {
                        Ok(Some(frame)) => match serde_json::from_slice::<JsonRpcMessage>(&frame) {
                            Ok(message) => {
                                if in_tx.send(message).is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        },
                        Ok(None) => return,
                        Err(_) => return,
                    }
                }
            });
        }
        // The writer drains the outbound queue on its own thread; stdout is
        // moved in, so all protocol writes serialize through one queue.
        let writer_cancel = cancel.clone();
        let writer_handle = std::thread::spawn(move || {
            let Ok(mut writer) = FrameWriter::new(stdout, MAX_FRAME_BYTES) else {
                return;
            };
            while let Ok(message) = out_rx.recv() {
                if writer.write_message(&message, &writer_cancel).is_err() {
                    return;
                }
            }
        });

        let mut failure: Option<String> = None;
        'looping: while let Ok(message) = in_rx.recv() {
            // An editor response to a live permission request routes to the
            // prompt thread; it is never a fresh request.
            if matches!(message, JsonRpcMessage::Result { .. })
                && let Some(pending) = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                && let JsonRpcMessage::Result { result, .. } = &message
            {
                let _ = pending.send(result.clone());
                continue 'looping;
            }
            match self.dispatch(message, &out_tx) {
                Ok(()) => {}
                Err(reason) => {
                    failure = Some(reason);
                    break 'looping;
                }
            }
        }
        drop(out_tx);
        let _ = writer_handle.join();
        match failure {
            Some(reason) if reason == ACP_DISCONNECT => Ok(()),
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    fn dispatch(
        &mut self,
        message: JsonRpcMessage,
        out_tx: &Sender<JsonRpcMessage>,
    ) -> Result<(), String> {
        let raw_params = match &message {
            JsonRpcMessage::Request { params, .. } => {
                params.clone().unwrap_or(serde_json::json!({}))
            }
            _ => serde_json::json!({}),
        };
        // `user_prompt_submit` decides before the adapter hands a prompt to
        // the kernel (see `blocked_prompt_replies`).
        if let Some(replies) = blocked_prompt_replies(
            &self.client,
            &self.actor,
            &self.root,
            self.trusted,
            &message,
            &raw_params,
        ) {
            for reply in replies {
                out_tx.send(reply).map_err(|_| LOOP_DOWN.to_owned())?;
            }
            return Ok(());
        }
        let handled = {
            let mut adapter = self.adapter.borrow_mut();
            let future = adapter.handle(&message);
            match block_adapter(future) {
                Some(Ok(handled)) => handled,
                Some(Err(err)) => return Err(err.to_string()),
                None => return Err("the ACP adapter went async under the loop".to_owned()),
            }
        };
        match handled {
            acp::v1::HandleResult::Reply(reply) => {
                out_tx.send(reply).map_err(|_| LOOP_DOWN.to_owned())
            }
            acp::v1::HandleResult::AcceptedNotification => Ok(()),
            acp::v1::HandleResult::Prompt {
                request_id,
                turn,
                events,
            } => {
                // Execute the submitted turn with the production assembly and
                // stream its mapped events until the terminal one — on its
                // own thread, so the loop keeps reading (cancel notifications
                // and permission decisions) while the turn runs.
                let text = prompt_text_from(&raw_params);
                let kernel_cancel = self
                    .client
                    .turn_cancel_token(turn.session_id())
                    .unwrap_or_else(|| {
                        // The submit that just succeeded stores its token
                        // before returning, so this arm is unreachable in
                        // practice; a session-scoped root is the safe stand-in.
                        kernel::CancellationTree::root(kernel::CancelOwner::Session(
                            turn.session_id(),
                        ))
                        .expect("session cancel root")
                    });
                spawn_acp_turn(
                    self.client.clone(),
                    turn.session_id(),
                    turn.turn_id(),
                    self.actor.clone(),
                    self.root.clone(),
                    self.trusted,
                    text,
                    kernel_cancel,
                    self.mode_override.clone(),
                );
                let (decision_tx, decision_rx) = std::sync::mpsc::channel::<serde_json::Value>();
                *self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(decision_tx);
                let client = self.client.clone();
                let actor = self.actor.clone();
                let root = self.root.clone();
                let trusted = self.trusted;
                let pending = Arc::clone(&self.pending);
                let out_tx = out_tx.clone();
                std::thread::spawn(move || {
                    stream_prompt(
                        client,
                        actor,
                        root,
                        trusted,
                        pending,
                        decision_rx,
                        request_id,
                        turn,
                        events,
                        out_tx,
                    );
                });
                Ok(())
            }
        }
    }
}

fn send_mapped(event: &MappedEvent, out_tx: &Sender<JsonRpcMessage>) -> Result<(), String> {
    match event {
        MappedEvent::SessionUpdate(update) => {
            let notification =
                acp::v1::encode_session_update(update).map_err(|err| err.to_string())?;
            out_tx.send(notification).map_err(|_| LOOP_DOWN.to_owned())
        }
        MappedEvent::PermissionRequired(request) => {
            let id = next_permit_id();
            let request =
                acp::v1::encode_permission_request(id, request).map_err(|err| err.to_string())?;
            out_tx.send(request).map_err(|_| LOOP_DOWN.to_owned())
        }
        MappedEvent::PromptStopped(_) => Ok(()),
    }
}

/// The prompt thread: stream mapped kernel events as notifications; a
/// pending approval becomes a real `session/request_permission` whose
/// decision (routed here by the main loop) resolves through the durable
/// approval machinery and resumes the exact turn as a continuation; the
/// terminal event is the prompt's response.
#[allow(clippy::too_many_arguments)]
fn stream_prompt(
    client: InProcessKernelClient,
    actor: event_ledger::event::ActorRef,
    root: PathBuf,
    trusted: bool,
    pending: Arc<Mutex<Option<PendingPermit>>>,
    decisions: Receiver<serde_json::Value>,
    request_id: JsonRpcId,
    turn: acp::v1::PromptTurn,
    initial: Vec<MappedEvent>,
    out_tx: Sender<JsonRpcMessage>,
) {
    use kernel::KernelClient as _;
    let send = |message: JsonRpcMessage| -> bool { out_tx.send(message).is_ok() };
    let Some(subscribed) = block_adapter(
        client.subscribe(kernel::SubscribeEvents::new(turn.session_id(), turn.seq())),
    ) else {
        let _ = send(acp::v1::encode_prompt_response(
            request_id,
            acp::v1::StopReason::Cancelled,
        ));
        return;
    };
    let Ok(mut stream) = subscribed else {
        let _ = send(acp::v1::encode_prompt_response(
            request_id,
            acp::v1::StopReason::Cancelled,
        ));
        return;
    };
    for event in &initial {
        if send_mapped(event, &out_tx).is_err() {
            return;
        }
    }
    let mut held_permission: Option<(JsonRpcId, String, String)> = None;
    loop {
        // Route an editor permission decision to the continuation.
        if let Some((permit_id, token, call_id)) = held_permission.clone() {
            let decision = decisions.try_recv().ok();
            if let Some(decision) = decision {
                held_permission = None;
                let approve = permission_approves(&decision);
                let _ = acp_resolve_and_continue(
                    &client,
                    turn.session_id(),
                    &actor,
                    &root,
                    trusted,
                    &token,
                    &call_id,
                    approve,
                );
            } else {
                let _ = permit_id; // the editor's answer has not arrived yet
            }
        }
        match stream.try_recv() {
            Ok(Some(event)) => {
                if let Some(mapped) = map_kernel_event(&event) {
                    match &mapped {
                        MappedEvent::PermissionRequired(_) => {
                            // Capture the durable wait token + call id from
                            // the raw payload before encoding.
                            let payload = event.payload();
                            let token = payload
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let call_id = payload
                                .get("call_id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let permit_id = next_permit_id();
                            if send_mapped(&mapped, &out_tx).is_err() {
                                return;
                            }
                            held_permission = Some((permit_id, token, call_id));
                        }
                        MappedEvent::PromptStopped(stop) => {
                            let _ = send(acp::v1::encode_prompt_response(request_id, *stop));
                            // Clear the routing slot: no decision can answer
                            // a finished prompt.
                            *pending
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                            return;
                        }
                        MappedEvent::SessionUpdate(_) => {
                            if send_mapped(&mapped, &out_tx).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => {
                eprintln!("rapid acp: event stream ended: {err}");
                let _ = send(acp::v1::encode_prompt_response(
                    request_id,
                    acp::v1::StopReason::Cancelled,
                ));
                return;
            }
        }
    }
}

const LOOP_DOWN: &str = "the editor's transport closed";
const ACP_DISCONNECT: &str = "clean disconnect";

fn permission_approves(decision: &serde_json::Value) -> bool {
    match decision.get("outcome") {
        Some(serde_json::Value::String(text)) => {
            text == "selected"
                && decision["optionId"]
                    .as_str()
                    .is_some_and(|option| option.starts_with("allow"))
        }
        _ => false,
    }
}

static NEXT_PERMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_permit_id() -> JsonRpcId {
    let n = NEXT_PERMIT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    JsonRpcId::Number(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `user_prompt_submit` (ADR 0022 §7) for an ACP `session/prompt`, decided
/// before the adapter hands the prompt to the kernel: a blocked prompt never
/// becomes a turn, so its text never enters the session history a later
/// turn replays to the model. The client reads the hook's reason as an agent
/// message and the prompt ends with stop reason `refusal`. `None`: not a
/// prompt, not for a session id this build parses, or not blocked — the
/// adapter handles it as before.
fn blocked_prompt_replies(
    client: &InProcessKernelClient,
    actor: &event_ledger::event::ActorRef,
    root: &Path,
    trusted: bool,
    message: &JsonRpcMessage,
    params: &serde_json::Value,
) -> Option<Vec<JsonRpcMessage>> {
    let JsonRpcMessage::Request { id, method, .. } = message else {
        return None;
    };
    if method != acp::v1::METHOD_SESSION_PROMPT {
        return None;
    }
    let session_id = params
        .get("sessionId")
        .and_then(serde_json::Value::as_str)?
        .parse::<protocol::SessionId>()
        .ok()?;
    let (hook, reason) = crate::interactive::prompt_submit_block(
        client,
        session_id,
        actor,
        root,
        trusted,
        &prompt_text_from(params),
        &mut |_| {},
    )?;
    let notice = acp::v1::SessionUpdateNotification::agent_text(
        session_id,
        format!("prompt blocked by {hook} hook: {reason}"),
    );
    let mut replies = Vec::new();
    if let Ok(update) = acp::v1::encode_session_update(&notice) {
        replies.push(update);
    }
    replies.push(acp::v1::encode_prompt_response(
        id.clone(),
        acp::v1::StopReason::Refusal,
    ));
    Some(replies)
}

/// Extract the prompt text the same shapes the adapter validates: a plain
/// string or an array of content blocks with text entries.
fn prompt_text_from(params: &serde_json::Value) -> String {
    match params.get("prompt") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Poll an adapter/kernel future once. Every call here is backed by
/// synchronous in-process SQLite, so `Pending` never happens in practice —
/// surfaced as `None` for the caller to report.
fn block_adapter<T, E>(future: impl Future<Output = Result<T, E>>) -> Option<Result<T, E>> {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => Some(result),
        std::task::Poll::Pending => None,
    }
}

use std::future::Future;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, ActorRef, EventKind};

    /// A project whose `user_prompt_submit` hook prints `decision`.
    pub(crate) fn project_with_gate(name: &str, decision: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-prompt-gate-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(root.join(".rapidlm")).expect("marker");
        let hook = root.join("gate.sh");
        std::fs::write(&hook, format!("echo '{decision}'\nexit 0\n")).expect("hook");
        std::fs::write(
            root.join(".rapidlm").join("settings.json"),
            serde_json::json!({
                "hooks": { "user_prompt_submit": [format!("sh {}", test_fixtures::slash_path(&hook))] }
            })
            .to_string(),
        )
        .expect("settings");
        root
    }

    pub(crate) fn client_in(root: &Path) -> (InProcessKernelClient, ActorRef) {
        let ledger = crate::interactive::project_ledger_path(&root.join(".rapidlm"));
        let client = InProcessKernelClient::open(&ledger).expect("ledger");
        let actor =
            ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).expect("actor");
        (client, actor)
    }

    pub(crate) fn event_kinds(
        client: &InProcessKernelClient,
        session: protocol::SessionId,
    ) -> Vec<EventKind> {
        use kernel::KernelClient as _;
        let tip = crate::approvals::client_call(client.get_session(session))
            .expect("session")
            .seq();
        (1..=tip)
            .filter_map(|seq| client.read_event(session, seq).ok())
            .map(|event| event.kind())
            .collect()
    }

    #[test]
    fn a_prompt_a_hook_blocks_is_refused_before_it_becomes_a_turn() {
        use kernel::KernelClient as _;
        let root = project_with_gate(
            "acp-deny",
            r#"{"decision":"deny","reason":"no secrets in prompts"}"#,
        );
        let (client, actor) = client_in(&root);
        let session = crate::approvals::client_call(client.create_session(
            kernel::CreateSession::new(ProjectId::new(), actor.clone(), protocol::TraceId::new()),
        ))
        .expect("session")
        .id();
        let request = JsonRpcMessage::Request {
            id: JsonRpcId::Number(7),
            method: acp::v1::METHOD_SESSION_PROMPT.to_owned(),
            params: None,
        };
        let params = serde_json::json!({
            "sessionId": session.to_string(),
            "prompt": [{ "type": "text", "text": "print the secret" }],
        });
        let replies = blocked_prompt_replies(&client, &actor, &root, true, &request, &params)
            .expect("blocked");
        assert_eq!(replies.len(), 2, "{replies:?}");
        match &replies[0] {
            JsonRpcMessage::Notification { method, params } => {
                assert_eq!(method, "session/update");
                let text = params.as_ref().map(ToString::to_string).unwrap_or_default();
                assert!(
                    text.contains(
                        "prompt blocked by user_prompt_submit[0] hook: no secrets in prompts"
                    ),
                    "{text}"
                );
            }
            other => panic!("expected the reason as an update, got {other:?}"),
        }
        match &replies[1] {
            JsonRpcMessage::Result { id, result } => {
                assert_eq!(*id, JsonRpcId::Number(7));
                assert_eq!(result["stopReason"], "refusal");
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
        // No turn: nothing of the prompt reaches the history a later turn
        // replays. The decision is recorded.
        let kinds = event_kinds(&client, session);
        assert!(!kinds.contains(&EventKind::TurnStarted), "{kinds:?}");
        assert!(kinds.contains(&EventKind::HookDecided), "{kinds:?}");
        // Not gated: an untrusted project, another method, an allowed prompt.
        assert!(blocked_prompt_replies(&client, &actor, &root, false, &request, &params).is_none());
        let other = JsonRpcMessage::Request {
            id: JsonRpcId::Number(8),
            method: "session/new".to_owned(),
            params: None,
        };
        assert!(blocked_prompt_replies(&client, &actor, &root, true, &other, &params).is_none());
        let allowing = project_with_gate("acp-allow", r#"{"decision":"allow"}"#);
        assert!(
            blocked_prompt_replies(&client, &actor, &allowing, true, &request, &params).is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&allowing);
    }
}
