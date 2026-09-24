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
//!   executes once, never the completed side effects. The prompt stays in
//!   flight across every approval its turn and continuations raise; it ends
//!   with the terminal event of the last continuation, or on a cancel.
//!
//! Frames are newline-delimited JSON-RPC over stdio (`crates/acp::stdio`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use acp::stdio::{FrameReader, FrameWriter, JsonRpcId, JsonRpcMessage, MAX_FRAME_BYTES};
use acp::v1::{MappedEvent, V1Adapter, map_kernel_event};
use kernel::InProcessKernelClient;
use protocol::ProjectId;

use crate::interactive::{acp_resolve_and_continue, spawn_acp_turn};

/// Where the editor's answer to one `session/request_permission` goes: the
/// waiting prompt thread, told which request it answers.
type PendingPermit = Sender<(JsonRpcId, serde_json::Value)>;

/// Every outstanding permission request, keyed by the JSON-RPC id it was
/// sent under. A prompt thread registers the id before the request is
/// written, so an answer can never arrive ahead of its route.
type PendingPermits = Arc<Mutex<HashMap<JsonRpcId, PendingPermit>>>;

/// Each in-flight prompt's cancel flag, by session, set by
/// `session/cancel`. A turn paused on an approval has released its lease,
/// so the kernel has no live turn to interrupt: the prompt thread reads
/// this flag instead.
type PromptCancels = Arc<Mutex<HashMap<protocol::SessionId, Arc<AtomicBool>>>>;

pub const ACP_USAGE: &str = "\
usage: rapid acp

Serve RapidLM as an ACP (Agent Client Protocol) agent over stdio, for
editors and any other client speaking ACP v1/v2.

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
        pending: PendingPermits::default(),
        cancels: PromptCancels::default(),
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
    /// Where each outstanding permission request's answer goes, by request
    /// id. An entry lives from the request's send to its answer (or its
    /// prompt's end).
    pending: PendingPermits,
    /// The cancel flag of each session's in-flight prompt.
    cancels: PromptCancels,
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
            // An editor response answers one of the serve's permission
            // requests: it routes to the prompt thread waiting on that id and
            // is never a fresh request. One nothing waits for any more (its
            // prompt already ended) asks nothing of the serve and is dropped.
            if let JsonRpcMessage::Result { id, .. } | JsonRpcMessage::Error { id, .. } = &message {
                let route = self
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
                match (route, message) {
                    (Some(route), JsonRpcMessage::Result { id, result }) => {
                        let _ = route.send((id, result));
                    }
                    // An error answer approves nothing.
                    (Some(route), JsonRpcMessage::Error { id, .. }) => {
                        let _ = route.send((id, serde_json::Value::Null));
                    }
                    _ => eprintln!("rapid acp: dropped a response no request awaits"),
                }
                continue 'looping;
            }
            // Flagged before the adapter interrupts: a turn paused on an
            // approval has no live turn for the kernel to stop.
            if let JsonRpcMessage::Notification { method, params } = &message
                && method == acp::v1::METHOD_SESSION_CANCEL
            {
                flag_cancel(&self.cancels, params.as_ref());
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
        let ready = self.adapter.borrow().is_ready();
        if let Some(replies) = ready
            .then(|| {
                blocked_prompt_replies(
                    &self.client,
                    &self.actor,
                    &self.root,
                    self.trusted,
                    &message,
                    &raw_params,
                )
            })
            .flatten()
        {
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
                // Registered before the loop reads another frame, so a
                // `session/cancel` right behind this prompt finds it.
                let cancelled = Arc::new(AtomicBool::new(false));
                self.cancels
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(turn.session_id(), Arc::clone(&cancelled));
                let routes = PromptRoutes {
                    pending: Arc::clone(&self.pending),
                    cancels: Arc::clone(&self.cancels),
                    session_id: turn.session_id(),
                    cancelled,
                    held: None,
                };
                let client = self.client.clone();
                let actor = self.actor.clone();
                let root = self.root.clone();
                let trusted = self.trusted;
                let out_tx = out_tx.clone();
                std::thread::spawn(move || {
                    stream_prompt(
                        client, actor, root, trusted, routes, request_id, turn, events, out_tx,
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

/// The approval a prompt is waiting on: the request id the editor answers,
/// and the durable wait it resolves.
struct HeldPermission {
    id: JsonRpcId,
    token: String,
    call_id: String,
    /// The turn has recorded its pause (lease released, suspension
    /// written): only now can a continuation resume it.
    paused: bool,
}

/// A prompt thread's routing state, released on every exit: its
/// outstanding permission route (a later answer is dropped, never
/// misrouted) and its cancel flag (unless a newer prompt of the session
/// already replaced it).
struct PromptRoutes {
    pending: PendingPermits,
    cancels: PromptCancels,
    session_id: protocol::SessionId,
    cancelled: Arc<AtomicBool>,
    held: Option<HeldPermission>,
}

impl PromptRoutes {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Hold `held`: answers to its request id route to `decisions`.
    fn hold(&mut self, held: HeldPermission, decisions: &PendingPermit) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(replaced) = self.held.take() {
            pending.remove(&replaced.id);
        }
        pending.insert(held.id.clone(), decisions.clone());
        self.held = Some(held);
    }
}

impl Drop for PromptRoutes {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&held.id);
        }
        let mut cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        if cancels
            .get(&self.session_id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &self.cancelled))
        {
            cancels.remove(&self.session_id);
        }
    }
}

/// `session/cancel`: flag the session's in-flight prompt, if any.
fn flag_cancel(cancels: &PromptCancels, params: Option<&serde_json::Value>) {
    let Some(session_id) = params
        .and_then(|params| params.get("sessionId"))
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse::<protocol::SessionId>().ok())
    else {
        return;
    };
    if let Some(flag) = cancels
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&session_id)
    {
        flag.store(true, Ordering::SeqCst);
    }
}

/// The kernel's record of a turn that paused itself on a pending approval
/// (`TurnOutcome::Waiting`): `turn.interrupted`, reason `approval_pending`.
fn is_approval_pause(event: &event_ledger::event::ErasedEventEnvelope) -> bool {
    event.kind() == event_ledger::event::EventKind::TurnInterrupted
        && event
            .payload()
            .get("reason")
            .and_then(serde_json::Value::as_str)
            == Some(kernel::InterruptReason::ApprovalPending.as_str())
}

/// The prompt thread: stream mapped kernel events as notifications; a
/// pending approval becomes a real `session/request_permission` whose
/// decision (routed here by the main loop) resolves through the durable
/// approval machinery and resumes the exact turn as a continuation — as
/// many times as the continuations ask; the terminal event is the prompt's
/// response.
#[allow(clippy::too_many_arguments)]
fn stream_prompt(
    client: InProcessKernelClient,
    actor: event_ledger::event::ActorRef,
    root: PathBuf,
    trusted: bool,
    mut routes: PromptRoutes,
    request_id: JsonRpcId,
    turn: acp::v1::PromptTurn,
    initial: Vec<MappedEvent>,
    out_tx: Sender<JsonRpcMessage>,
) {
    use kernel::KernelClient as _;
    let send = |message: JsonRpcMessage| -> bool { out_tx.send(message).is_ok() };
    let (decision_tx, decisions): (PendingPermit, Receiver<(JsonRpcId, serde_json::Value)>) =
        std::sync::mpsc::channel();
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
    loop {
        // A turn paused on the held approval waits for the editor: its
        // answer resumes it as a continuation; a cancel ends the prompt.
        if routes.held.as_ref().is_some_and(|held| held.paused) {
            if routes.is_cancelled() {
                // Nothing runs; the approval stays pending in the ledger,
                // resumable from any surface that lists approvals.
                let _ = send(acp::v1::encode_prompt_response(
                    request_id,
                    acp::v1::StopReason::Cancelled,
                ));
                return;
            }
            while let Ok((answered, decision)) = decisions.try_recv() {
                if routes.held.as_ref().is_none_or(|held| held.id != answered) {
                    continue; // an answer to a request this prompt no longer holds
                }
                let Some(held) = routes.held.take() else {
                    break;
                };
                let resumed = acp_resolve_and_continue(
                    &client,
                    turn.session_id(),
                    &actor,
                    &root,
                    trusted,
                    &held.token,
                    &held.call_id,
                    permission_approves(&decision),
                );
                if let Err(reason) = resumed {
                    // No continuation runs, so nothing is left to stream.
                    eprintln!("rapid acp: the paused turn could not resume: {reason}");
                    let _ = send(acp::v1::encode_prompt_response(
                        request_id,
                        acp::v1::StopReason::Refusal,
                    ));
                    return;
                }
                // A cancel that landed while no turn was live interrupted
                // nothing; the continuation is live now.
                if routes.is_cancelled() {
                    let _ = block_adapter(client.interrupt(kernel::Interrupt::new(
                        turn.session_id(),
                        kernel::InterruptReason::ClientRequested,
                        actor.clone(),
                        protocol::TraceId::new(),
                    )));
                }
                break;
            }
        }
        match stream.try_recv() {
            Ok(Some(event)) => {
                // The turn paused on the approval this prompt holds: not the
                // end of the prompt — the continuation its answer resumes is.
                if is_approval_pause(&event)
                    && let Some(held) = routes.held.as_mut()
                {
                    held.paused = true;
                    continue;
                }
                if let Some(mapped) = map_kernel_event(&event) {
                    match &mapped {
                        MappedEvent::PermissionRequired(request) => {
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
                            // The route exists before the request is written,
                            // under the id the request carries.
                            let id = next_permit_id();
                            routes.hold(
                                HeldPermission {
                                    id: id.clone(),
                                    token,
                                    call_id,
                                    paused: false,
                                },
                                &decision_tx,
                            );
                            let Ok(request) = acp::v1::encode_permission_request(id, request)
                            else {
                                return;
                            };
                            if !send(request) {
                                return;
                            }
                        }
                        MappedEvent::PromptStopped(stop) => {
                            let _ = send(acp::v1::encode_prompt_response(request_id, *stop));
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
    // Only a prompt the adapter would accept is judged here: an unknown or
    // closed session is the adapter's error to report, and nothing is
    // recorded on it.
    {
        use kernel::KernelClient as _;
        let snapshot = crate::approvals::client_call(client.get_session(session_id)).ok()?;
        if snapshot.status() == kernel::SessionStatus::Closed {
            return None;
        }
    }
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

    fn serve_in(root: &Path) -> (Serve, InProcessKernelClient) {
        let (client, actor) = client_in(root);
        let adapter = V1Adapter::new(
            client.clone(),
            ProjectId::new(),
            actor.clone(),
            acp::stdio::CancellationToken::new(),
        );
        let serve = Serve {
            client: client.clone(),
            adapter: std::cell::RefCell::new(adapter),
            actor,
            root: root.to_path_buf(),
            trusted: true,
            pending: PendingPermits::default(),
            cancels: PromptCancels::default(),
            mode_override: Arc::new(Mutex::new(None)),
        };
        (serve, client)
    }

    fn request(id: i64, method: &str, params: serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::Request {
            id: JsonRpcId::Number(id),
            method: method.to_owned(),
            params: Some(params),
        }
    }

    #[test]
    fn dispatch_refuses_a_blocked_prompt_only_where_the_adapter_would_accept_it() {
        let root = project_with_gate(
            "acp-dispatch",
            r#"{"decision":"deny","reason":"no secrets in prompts"}"#,
        );
        let (mut serve, client) = serve_in(&root);
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let prompt = |session: &str| {
            request(
                3,
                acp::v1::METHOD_SESSION_PROMPT,
                serde_json::json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": "print the secret" }],
                }),
            )
        };
        // Before `initialize`: the adapter's own refusal, not the gate's —
        // even for a session that exists.
        let existing = {
            use kernel::KernelClient as _;
            crate::approvals::client_call(client.create_session(kernel::CreateSession::new(
                ProjectId::new(),
                serve.actor.clone(),
                protocol::TraceId::new(),
            )))
            .expect("session")
            .id()
            .to_string()
        };
        let unknown = protocol::SessionId::new().to_string();
        let _ = serve.dispatch(prompt(&existing), &out_tx);
        assert!(
            !out_rx
                .try_iter()
                .any(|reply| matches!(&reply, JsonRpcMessage::Result { result, .. } if result["stopReason"] == "refusal")),
            "not gated before initialize"
        );
        serve
            .dispatch(
                request(
                    1,
                    acp::v1::METHOD_INITIALIZE,
                    serde_json::json!({ "protocolVersion": 1 }),
                ),
                &out_tx,
            )
            .expect("initialize");
        serve
            .dispatch(
                request(
                    2,
                    acp::v1::METHOD_SESSION_NEW,
                    serde_json::json!({ "cwd": root.display().to_string(), "mcpServers": [] }),
                ),
                &out_tx,
            )
            .expect("session/new");
        let session = out_rx
            .try_iter()
            .find_map(|reply| match reply {
                JsonRpcMessage::Result {
                    id: JsonRpcId::Number(2),
                    result,
                } => result["sessionId"].as_str().map(str::to_owned),
                _ => None,
            })
            .expect("a session id");
        serve
            .dispatch(prompt(&session), &out_tx)
            .expect("dispatched");
        let replies: Vec<JsonRpcMessage> = out_rx.try_iter().collect();
        assert!(
            replies.iter().any(|reply| matches!(
                reply,
                JsonRpcMessage::Result { id: JsonRpcId::Number(3), result } if result["stopReason"] == "refusal"
            )),
            "{replies:?}"
        );
        let kinds = event_kinds(&client, session.parse().expect("id"));
        assert!(!kinds.contains(&EventKind::TurnStarted), "{kinds:?}");
        // An unknown session: the adapter's error, nothing recorded anywhere.
        let _ = serve.dispatch(prompt(&unknown), &out_tx);
        assert!(!out_rx.try_iter().any(|reply| matches!(
            &reply,
            JsonRpcMessage::Result { result, .. } if result["stopReason"] == "refusal"
        )));
        let _ = std::fs::remove_dir_all(&root);
    }
}
