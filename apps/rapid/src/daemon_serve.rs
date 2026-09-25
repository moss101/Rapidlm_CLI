//! `rapid daemon` — bind the kernel's session/turn services on a Unix socket
//! for the TypeScript SDK (`sdk/typescript`).
//!
//! The kernel's own `IpcServer` speaks underscore method names with an
//! `auth.challenge` handshake; the SDK speaks dotted names with a `hello` /
//! `hello_ok` handshake (`rapidlm.sdk.rpc` v1). This module is the bridge:
//! one long-lived process bound to a workspace, serving the SDK's exact wire
//! contract over a newline-delimited JSON Unix socket:
//!
//! - `sessions.create/get/fork/rewind` → the kernel client's session ops,
//!   returning the same wire shapes the SDK's generated decoders accept
//!   (unknown fields are rejected there, so the shapes here are exact);
//! - `turns.submit` → kernel submit **plus execution** with the production
//!   assembly (hooks, MCP, retrieval, durable approval sink, worktree-
//!   isolated subagents) — the SDK sees progress by subscribing to events;
//! - `turns.interrupt` → the kernel's live-turn cancel;
//! - `approvals.resolve` → the durable approval machinery: the wait row is
//!   marked terminal and the paused turn resumes as a continuation (the
//!   SDK names a turn-scoped `expected_seq`; the daemon resolves the
//!   session's oldest pending wait, which is that turn's, once and by its
//!   token — a stale `expected_seq` or a session with nothing pending is
//!   refused and writes nothing);
//! - `events.subscribe` → `{kind:"event"}` frames streamed from the session
//!   cursor, ended by `stream_end` on cancel or terminal.
//!
//! Boundaries: the socket lives in the project's `.rapidlm/` directory with
//! owner-only permissions; a `hello` carrying an auth handle is verified
//! against the local daemon token (`~/.rapidlm/daemon.token`) when one
//! exists — a wrong token is rejected before any RPC is served.

// Everything below the usage text serves the Unix socket; on Windows the
// daemon path is a typed refusal, so the whole import block is unix-only
// or it is dead there (the failure ci.yml caught twice).
#[cfg(unix)]
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
// PathBuf is NOT unix-only: `run_daemon` parses `--socket` on every
// platform before the cfg(unix) refusal.
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, Sender};
#[cfg(unix)]
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use acp::stdio::{FrameReader, MAX_FRAME_BYTES};
#[cfg(unix)]
use kernel::InProcessKernelClient;
#[cfg(unix)]
use protocol::{ProjectId, SessionId};

#[cfg(unix)]
use crate::interactive::{acp_resolve_and_continue, spawn_acp_turn};

pub const DAEMON_USAGE: &str = "\
usage: rapid daemon [--socket <path>]

Bind the kernel's session/turn services on a Unix socket for the TypeScript
SDK. The socket defaults to <project>/.rapidlm/daemon.sock and is created
owner-only; the path is printed on startup. One daemon serves one workspace.

Exit codes: 0 clean shutdown · 2 usage · 1 bind/serve failure.
";

/// `rapid daemon`.
// On non-Unix the cfg(not(unix)) refusal block ends in a `return` that is
// the function tail there (the Unix `serve_unix` tail is cfg'd out), which
// needless_return would flag.
#[cfg_attr(not(unix), allow(clippy::needless_return))]
pub fn run_daemon(args: &[String]) -> Result<i32, crate::p9_commands::P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{DAEMON_USAGE}");
        return Ok(0);
    }
    let socket = match args.iter().position(|arg| arg == "--socket") {
        Some(position) => args
            .get(position + 1)
            .map(PathBuf::from)
            .ok_or(crate::p9_commands::P9CommandError::Usage)?,
        None => crate::interactive::project_ledger_path(
            &std::env::current_dir()
                .unwrap_or_default()
                .join(crate::interactive::PROJECT_MARKER),
        )
        .parent()
        .map(|dir| dir.join("daemon.sock"))
        .unwrap_or_else(|| PathBuf::from("daemon.sock")),
    };
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid daemon: no project workspace resolved");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };
    if !trusted {
        eprintln!(
            "warning: the project is not trusted; sessions served here will refuse every \
tool call. Approve trust with `rapid trust grant`."
        );
    }
    let ledger_path =
        crate::interactive::project_ledger_path(&root.join(crate::interactive::PROJECT_MARKER));
    if let Some(parent) = ledger_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The daemon's wire transport is a Unix domain socket. On platforms
    // without one this is a typed runtime refusal, not a build failure —
    // the SDK path remains Unix-first by documented platform statement.
    #[cfg(not(unix))]
    {
        let _ = &socket;
        eprintln!(
            "rapid daemon: Unix domain sockets are not available on this platform; \
the SDK daemon path is Unix-first (see the platform notes in docs/getting-started.md)"
        );
        return Err(crate::p9_commands::P9CommandError::Agent(
            "daemon requires a Unix socket; unsupported on this platform".to_owned(),
        ));
    }
    #[cfg(unix)]
    serve_unix(&socket, &ledger_path, &root, trusted)
}

/// The Unix-socket serving loop: bind, tighten permissions, accept SDK
/// clients until the listener breaks. Only compiled on Unix.
#[cfg(unix)]
fn serve_unix(
    socket: &std::path::Path,
    ledger_path: &std::path::Path,
    root: &std::path::Path,
    trusted: bool,
) -> Result<i32, crate::p9_commands::P9CommandError> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|err| {
        crate::p9_commands::P9CommandError::Agent(format!("bind {socket:?}: {err}"))
    })?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    println!("rapid daemon listening on {}", socket.display());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let client = InProcessKernelClient::open(ledger_path)
        .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;
    let actor = event_ledger::event::ActorRef::new(
        event_ledger::event::ActorKind::Agent,
        &protocol::EventId::new().to_string(),
    )
    .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;
    let daemon_token = std::fs::read_to_string(user_home().join("daemon.token"))
        .map(|token| token.trim().to_owned())
        .ok();

    for stream in listener.incoming() {
        let Ok(stream) = stream else { break };
        let serve = Connection {
            client: client.clone(),
            actor: actor.clone(),
            root: root.to_path_buf(),
            trusted,
            daemon_token: daemon_token.clone(),
            mode_override: Default::default(),
        };
        std::thread::spawn(move || {
            let _ = serve.serve(stream);
        });
    }
    let _ = std::fs::remove_file(socket);
    Ok(0)
}

#[cfg(unix)]
fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// One connected SDK client.
#[cfg(unix)]
struct Connection {
    client: InProcessKernelClient,
    actor: event_ledger::event::ActorRef,
    root: PathBuf,
    trusted: bool,
    daemon_token: Option<String>,
    /// The connection's permission-mode override cell: every spawned turn
    /// reads it. Unset by default (mode resolves from env/settings); the
    /// cell exists so a mode switch surface shares one authoritative cell
    /// per connection.
    mode_override: std::sync::Arc<std::sync::Mutex<Option<crate::permissions::PermissionMode>>>,
}

#[cfg(unix)]
impl Connection {
    fn serve(&self, stream: std::os::unix::net::UnixStream) -> Result<(), String> {
        let _ = stream.set_nonblocking(false);
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3600)))
            .ok();
        let raw_writer = stream
            .try_clone()
            .map_err(|err| format!("socket clone: {err}"))?;
        let writer = Mutex::new(raw_writer);
        let mut reader =
            FrameReader::new(stream, MAX_FRAME_BYTES).map_err(|err| err.to_string())?;
        let cancel = acp::stdio::CancellationToken::new();
        // Newline-delimited JSON out: one `write_all` per frame so a frame
        // is never interleaved mid-line by another writer (see `write_frame`).

        // Handshake: exactly one hello, then RPCs.
        let hello = read_request(&mut reader, &cancel)?;
        let Some(reply_id) = self.verify_hello(&hello) else {
            write_frame(
                &writer,
                &error_frame(&hello_frame_id(&hello), "unauthorized", "bad daemon token"),
            )?;
            return Err("unauthorized hello".to_owned());
        };
        write_frame(&writer, &hello_ok(reply_id))?;

        // One event stream at a time; its cancel path is the `cancel` frame.
        let stream_control: Arc<Mutex<Option<Sender<StreamCommand>>>> = Arc::new(Mutex::new(None));
        loop {
            let frame = read_request(&mut reader, &cancel)?;
            let (_kind, id, method, params) = match parse_request(&frame) {
                Some(parsed) => parsed,
                None => {
                    if frame.get("kind").and_then(serde_json::Value::as_str) == Some("cancel") {
                        let target = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        if let Some(control) = stream_control
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                        {
                            let _ = control.send(StreamCommand::Stop);
                        }
                        let _ = target;
                        continue;
                    }
                    write_frame(
                        &writer,
                        &error_frame(&frame["id"], "invalid_request", "expected an RPC request"),
                    )?;
                    continue;
                }
            };
            if method == "events.subscribe" {
                let (tx, rx) = std::sync::mpsc::channel::<StreamCommand>();
                *stream_control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
                write_frame(&writer, &response_ok(&id, serde_json::json!({})))?;
                self.stream_events(&writer, &id, &params, rx)?;
                continue;
            }
            let outcome = self.rpc(&method, &params);
            let frame = match outcome {
                Ok(result) => response_ok(&id, result),
                Err(error) => error_frame(&id, "rpc_failed", &error),
            };
            write_frame(&writer, &frame)?;
        }
    }

    fn verify_hello(&self, hello: &serde_json::Value) -> Option<serde_json::Value> {
        if hello.get("schema").and_then(serde_json::Value::as_str) != Some("rapidlm.sdk.rpc") {
            return None;
        }
        if hello
            .get("schema_version")
            .and_then(serde_json::Value::as_i64)
            != Some(1)
        {
            return None;
        }
        if hello.get("kind").and_then(serde_json::Value::as_str) != Some("hello") {
            return None;
        }
        if let Some(expected) = &self.daemon_token {
            let presented = hello
                .get("auth")
                .and_then(|auth| auth.get("handle"))
                .and_then(serde_json::Value::as_str);
            if presented != Some(expected.as_str()) {
                return None;
            }
        }
        hello.get("id").cloned()
    }

    /// `turns.submit` up to the accepted turn: the `user_prompt_submit` gate
    /// (ADR 0022 §7), then the kernel submit at the client's `expected_seq`.
    /// The gate runs, and records, only on a session that can take a turn —
    /// an unknown or closed one is the kernel's error to report. A deny
    /// records and refuses: the prompt never becomes a turn, so its text never
    /// enters the history a later turn replays. Otherwise the decisions are
    /// recorded after the submit — first, they would move the session past
    /// `expected_seq` — and whether or not the submit was accepted: the hooks
    /// ran either way.
    fn submit_prompt(
        &self,
        session: SessionId,
        expected_seq: u64,
        text: &str,
        trace: protocol::TraceId,
    ) -> Result<kernel::TurnHandle, String> {
        use kernel::KernelClient as _;
        let snapshot = crate::approvals::client_call(self.client.get_session(session))
            .map_err(|err| err.to_string())?;
        let gate = (snapshot.status() != kernel::SessionStatus::Closed)
            .then(|| crate::interactive::prompt_submit_decision(&self.root, self.trusted, text))
            .flatten();
        let record = |report: &crate::hooks::PostHookReport| {
            crate::interactive::record_prompt_submit(
                &self.client,
                session,
                &self.actor,
                report,
                &mut |_| {},
            );
        };
        if let Some(report) = &gate
            && let Some((hook, reason)) = report.first_deny()
        {
            record(report);
            return Err(format!(
                "prompt blocked by {hook} hook: {reason} (the decision is recorded on the \
session, which moved: refresh the session before the next submit)"
            ));
        }
        let submitted =
            crate::approvals::client_call(self.client.submit_turn(kernel::SubmitTurn::new(
                session,
                expected_seq,
                self.actor.clone(),
                trace,
                text.to_owned(),
            )))
            .map_err(|err| err.to_string());
        if let Some(report) = &gate {
            record(report);
        }
        submitted
    }

    fn rpc(&self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        use kernel::KernelClient as _;
        match method {
            "sessions.create" => {
                let snapshot = crate::approvals::client_call(self.client.create_session(
                    kernel::CreateSession::new(
                        ProjectId::new(),
                        self.actor.clone(),
                        trace_id_of(params),
                    ),
                ))
                .map_err(|err| err.to_string())?;
                snapshot_json(&snapshot)
            }
            "sessions.get" => {
                let session = session_of(params)?;
                let snapshot = crate::approvals::client_call(self.client.get_session(session))
                    .map_err(|err| err.to_string())?;
                snapshot_json(&snapshot)
            }
            "sessions.fork" => {
                let source = uuid_field(params, "source")?;
                let at_seq = u64_field(params, "at_seq")?;
                let snapshot = crate::approvals::client_call(self.client.fork_session(
                    kernel::ForkSession::new(
                        source,
                        at_seq,
                        self.actor.clone(),
                        trace_id_of(params),
                    ),
                ))
                .map_err(|err| err.to_string())?;
                snapshot_json(&snapshot)
            }
            "sessions.rewind" => {
                let session = session_of(params)?;
                let to_seq = u64_field(params, "to_seq")?;
                let rewound = crate::approvals::client_call(
                    self.client
                        .rewind(kernel::RewindSession::new(session, to_seq)),
                )
                .map_err(|err| err.to_string())?;
                snapshot_json(rewound.snapshot())
            }
            "turns.submit" => {
                let session = session_of(params)?;
                let expected_seq = u64_field(params, "expected_seq")?;
                let text = params
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let handle =
                    self.submit_prompt(session, expected_seq, &text, trace_id_of(params))?;
                // Execute the turn with the production assembly; progress
                // streams to any events.subscribe consumer.
                let kernel_cancel = self.client.turn_cancel_token(session).unwrap_or_else(|| {
                    kernel::CancellationTree::root(kernel::CancelOwner::Session(session))
                        .expect("session cancel root")
                });
                spawn_acp_turn(
                    self.client.clone(),
                    session,
                    handle.turn_id(),
                    self.actor.clone(),
                    self.root.clone(),
                    self.trusted,
                    text,
                    kernel_cancel,
                    self.mode_override.clone(),
                    // The daemon does not wait on its turn threads.
                    std::sync::Arc::default(),
                );
                turn_handle_json(&handle)
            }
            "turns.interrupt" => {
                let session = session_of(params)?;
                crate::approvals::client_call(self.client.interrupt(kernel::Interrupt::new(
                    session,
                    kernel::InterruptReason::ClientRequested,
                    self.actor.clone(),
                    trace_id_of(params),
                )))
                .map_err(|err| err.to_string())?;
                Ok(serde_json::json!({}))
            }
            "approvals.resolve" => {
                let session = session_of(params)?;
                let expected_seq = u64_field(params, "expected_seq")?;
                let decision = match params.get("decision").and_then(serde_json::Value::as_str) {
                    Some("approved") => kernel::ApprovalDecision::Approved,
                    Some("denied") => kernel::ApprovalDecision::Denied,
                    other => return Err(format!("unknown decision {other:?}")),
                };
                // The SDK names the turn, not the wait; resolve the session's
                // oldest pending wait once, by its token, and resume it as a
                // continuation. Nothing pending is nothing to answer.
                let pendings =
                    crate::approvals::client_call(self.client.pending_approvals(session))?;
                let Some(pending) = pendings.first() else {
                    return Err("no pending approval to resolve in this session".to_owned());
                };
                let token = pending.payload().id.clone();
                let call_id = pending.payload().call_id.clone();
                // The resolution appends at the tip it reads, so the SDK's
                // view of the session is checked here; the wait, not this
                // check, is what keeps the answer single.
                let tip = crate::approvals::client_call(self.client.get_session(session))?.seq();
                if tip != expected_seq {
                    return Err(format!(
                        "the session moved past expected_seq {expected_seq} (it is at {tip}); \
refresh and answer again"
                    ));
                }
                if let Err(err) = acp_resolve_and_continue(
                    &self.client,
                    session,
                    &self.actor,
                    &self.root,
                    self.trusted,
                    &token,
                    &call_id,
                    decision == kernel::ApprovalDecision::Approved,
                    None,
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ) {
                    return Err(resolve_failure(
                        recorded_answer(&self.client, session, tip, &token),
                        decision,
                        &self.actor,
                        err,
                    ));
                }
                Ok(serde_json::json!({}))
            }
            other => Err(format!("unknown method {other:?}")),
        }
    }

    fn stream_events(
        &self,
        writer: &Mutex<std::os::unix::net::UnixStream>,
        id: &serde_json::Value,
        params: &serde_json::Value,
        control: Receiver<StreamCommand>,
    ) -> Result<(), String> {
        use kernel::KernelClient as _;
        let session = session_of(params)?;
        let from_seq = u64_field(params, "from_seq").unwrap_or(0);
        let Ok(subscribed) = crate::approvals::client_call(
            self.client
                .subscribe(kernel::SubscribeEvents::new(session, from_seq)),
        ) else {
            return Err("subscribe went async under the daemon".to_owned());
        };
        let mut stream = subscribed;
        loop {
            // Stop on an explicit cancel frame without blocking the reader.
            if let Ok(StreamCommand::Stop) = control.try_recv() {
                stream.close();
                break;
            }
            match stream.try_recv() {
                Ok(Some(event)) => {
                    let event_value =
                        serde_json::to_value(&event).map_err(|err| err.to_string())?;
                    let frame = serde_json::json!({
                        "schema": "rapidlm.sdk.rpc",
                        "schema_version": 1,
                        "kind": "event",
                        "id": id,
                        "cursor": event.seq(),
                        "event": event_value,
                    });
                    write_frame(writer, &frame)?;
                    // A terminal turn event ends the stream: the SDK's run()
                    // generator is done, and the connection thread must get
                    // back to reading requests or everything after the first
                    // subscribe starves.
                    if matches!(
                        event.kind(),
                        event_ledger::event::EventKind::TurnCompleted
                            | event_ledger::event::EventKind::TurnFailed
                            | event_ledger::event::EventKind::TurnInterrupted
                    ) {
                        let frame = serde_json::json!({
                            "schema": "rapidlm.sdk.rpc",
                            "schema_version": 1,
                            "kind": "stream_end",
                            "id": id,
                            "cursor": event.seq(),
                            "reason": "complete",
                        });
                        write_frame(writer, &frame)?;
                        return Ok(());
                    }
                }
                Ok(None) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_err) => {
                    let cursor = stream.cursor();
                    let frame = serde_json::json!({
                        "schema": "rapidlm.sdk.rpc",
                        "schema_version": 1,
                        "kind": "stream_end",
                        "id": id,
                        "cursor": cursor,
                        "reason": "disconnected",
                    });
                    write_frame(writer, &frame)?;
                    return Ok(());
                }
            }
        }
        let cursor = stream.cursor();
        let frame = serde_json::json!({
            "schema": "rapidlm.sdk.rpc",
            "schema_version": 1,
            "kind": "stream_end",
            "id": id,
            "cursor": cursor,
            "reason": "cancelled",
        });
        write_frame(writer, &frame)
    }
}

#[cfg(unix)]
enum StreamCommand {
    Stop,
}

/// One newline-terminated JSON frame, written atomically per frame.
#[cfg(unix)]
fn write_frame(
    writer: &Mutex<std::os::unix::net::UnixStream>,
    frame: &serde_json::Value,
) -> Result<(), String> {
    let mut line = serde_json::to_string(frame).map_err(|err| err.to_string())?;
    line.push('\n');
    let mut stream = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    stream
        .write_all(line.as_bytes())
        .map_err(|err| err.to_string())
}

#[cfg(unix)]
fn read_request(
    reader: &mut FrameReader<impl std::io::Read>,
    cancel: &acp::stdio::CancellationToken,
) -> Result<serde_json::Value, String> {
    let frame = reader.read_frame(cancel).map_err(|err| err.to_string())?;
    let Some(frame) = frame else {
        return Err("disconnected".to_owned());
    };
    serde_json::from_slice(&frame).map_err(|err| err.to_string())
}

#[cfg(unix)]
fn parse_request(
    frame: &serde_json::Value,
) -> Option<(String, serde_json::Value, String, serde_json::Value)> {
    let kind = frame.get("kind").and_then(serde_json::Value::as_str)?;
    if kind != "request" {
        return None;
    }
    if frame.get("schema").and_then(serde_json::Value::as_str) != Some("rapidlm.sdk.rpc") {
        return None;
    }
    Some((
        "request".to_owned(),
        frame.get("id")?.clone(),
        frame.get("method")?.as_str()?.to_owned(),
        frame
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({})),
    ))
}

#[cfg(unix)]
fn hello_ok(id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema": "rapidlm.sdk.rpc",
        "schema_version": 1,
        "kind": "hello_ok",
        "id": id,
        "wire_schema": "rapidlm.sdk.wire",
        "wire_schema_version": 1,
        "wire_schema_sha256": "53423d0293e15ca708cc2450e393a9d82bdd742e23ec50d8a508bffdd031823b",
    })
}

#[cfg(unix)]
fn response_ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema": "rapidlm.sdk.rpc",
        "schema_version": 1,
        "kind": "response",
        "id": id,
        "result": result,
    })
}

#[cfg(unix)]
fn error_frame(id: &serde_json::Value, code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "rapidlm.sdk.rpc",
        "schema_version": 1,
        "kind": "response",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(unix)]
fn hello_frame_id(hello: &serde_json::Value) -> serde_json::Value {
    hello.get("id").cloned().unwrap_or(serde_json::Value::Null)
}

#[cfg(unix)]
fn session_of(params: &serde_json::Value) -> Result<SessionId, String> {
    let raw = params
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing session_id")?;
    raw.parse::<SessionId>()
        .map_err(|_| format!("session_id {raw:?} is not a session id"))
}

#[cfg(unix)]
fn uuid_field(params: &serde_json::Value, field: &str) -> Result<SessionId, String> {
    let raw = params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing {field}"))?;
    raw.parse::<SessionId>()
        .map_err(|_| format!("{field} {raw:?} is not an id"))
}

#[cfg(unix)]
fn u64_field(params: &serde_json::Value, field: &str) -> Result<u64, String> {
    params
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("missing or invalid {field}"))
}

#[cfg(unix)]
fn trace_id_of(params: &serde_json::Value) -> protocol::TraceId {
    params
        .get("trace_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or_else(protocol::TraceId::new)
}

/// The exact wire shape `decodeSession` accepts (unknown fields are
/// rejected by the SDK, so this stays minimal and precise).
#[cfg(unix)]
fn snapshot_json(snapshot: &kernel::SessionSnapshot) -> Result<serde_json::Value, String> {
    serde_json::to_value(snapshot).map_err(|err| err.to_string())
}

/// The decision and actor of the `approval.resolved` recorded for `token`
/// after `from_seq`, if one was. A ledger that cannot be read answers
/// `None`, the same as no resolution: the caller then reports the resolver's
/// own error rather than claim either outcome.
#[cfg(unix)]
fn recorded_answer(
    client: &InProcessKernelClient,
    session: SessionId,
    from_seq: u64,
    token: &str,
) -> Option<(kernel::ApprovalDecision, event_ledger::event::ActorRef)> {
    use kernel::KernelClient as _;
    let tip = crate::approvals::client_call(client.get_session(session))
        .ok()?
        .seq();
    ((from_seq + 1)..=tip).find_map(|seq| {
        let event = client.read_event(session, seq).ok()?;
        if event.kind() != event_ledger::event::EventKind::ApprovalResolved {
            return None;
        }
        let payload: kernel::ApprovalResolvedPayload =
            serde_json::from_value(event.payload().clone()).ok()?;
        (payload.wait_token == token).then(|| (payload.decision, event.actor().clone()))
    })
}

/// What a failed `approvals.resolve` tells the SDK. A refused resolution
/// wrote nothing; a recorded one whose turn could not resume must read as
/// neither; and an answer another resolver recorded first is not this one.
#[cfg(unix)]
fn resolve_failure(
    recorded: Option<(kernel::ApprovalDecision, event_ledger::event::ActorRef)>,
    decision: kernel::ApprovalDecision,
    actor: &event_ledger::event::ActorRef,
    err: String,
) -> String {
    match recorded {
        Some((recorded, by)) if recorded == decision && &by == actor => {
            format!("the decision was recorded, but the paused turn could not resume: {err}")
        }
        Some(_) => format!("the approval was answered by another resolver first: {err}"),
        None => err,
    }
}

#[cfg(unix)]
fn turn_handle_json(handle: &kernel::TurnHandle) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "session_id": handle.session_id().to_string(),
        "turn_id": handle.turn_id().to_string(),
        "seq": handle.seq(),
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp_serve::tests::{client_in, event_kinds, project_with_gate};
    use event_ledger::event::EventKind;

    #[test]
    fn a_prompt_a_hook_blocks_is_refused_before_it_becomes_a_turn() {
        let root = project_with_gate(
            "daemon-deny",
            r#"{"decision":"deny","reason":"no secrets in prompts"}"#,
        );
        let (client, actor) = client_in(&root);
        let connection = Connection {
            client: client.clone(),
            actor,
            root: root.clone(),
            trusted: true,
            daemon_token: None,
            mode_override: Default::default(),
        };
        let snapshot = connection
            .rpc("sessions.create", &serde_json::json!({}))
            .expect("session");
        let session = snapshot["id"].as_str().expect("id").to_owned();
        let err = connection
            .rpc(
                "turns.submit",
                &serde_json::json!({
                    "session_id": session,
                    "expected_seq": snapshot["seq"],
                    "prompt": "print the secret",
                }),
            )
            .expect_err("blocked");
        assert!(
            err.contains("prompt blocked by user_prompt_submit[0] hook: no secrets in prompts"),
            "{err}"
        );
        let kinds = event_kinds(&client, session.parse().expect("session id"));
        assert!(
            !kinds.contains(&EventKind::TurnStarted),
            "no turn: {kinds:?}"
        );
        assert!(kinds.contains(&EventKind::HookDecided), "{kinds:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_allowed_prompt_submits_at_the_clients_seq_and_the_decision_lands_after_the_turn() {
        // A hook printing a v2 result records a decision; recorded before the
        // submit it moved the session past the client's `expected_seq` and
        // every submit in the project conflicted.
        let root = project_with_gate("daemon-allow", r#"{"decision":"allow"}"#);
        let (client, actor) = client_in(&root);
        let connection = Connection {
            client: client.clone(),
            actor,
            root: root.clone(),
            trusted: true,
            daemon_token: None,
            mode_override: Default::default(),
        };
        let snapshot = connection
            .rpc("sessions.create", &serde_json::json!({}))
            .expect("session");
        let session = snapshot["id"].as_str().expect("id").to_owned();
        // `submit_prompt`, not the whole `turns.submit`: that would spawn the
        // turn, which resolves a model from the real environment.
        let session_id: SessionId = session.parse().expect("session id");
        let handle = connection
            .submit_prompt(
                session_id,
                snapshot["seq"].as_u64().expect("seq"),
                "say hello",
                protocol::TraceId::new(),
            )
            .expect("an allowed prompt is submitted at the client's seq");
        let _ = client.finish_turn(kernel::FinishTurn::new(
            session_id,
            handle.turn_id(),
            connection.actor.clone(),
            protocol::TraceId::new(),
            kernel::TurnOutcome::Completed { text: None },
        ));
        // A stale seq: the kernel refuses the submit, and what the hooks
        // decided is recorded all the same — they ran.
        assert!(
            connection
                .submit_prompt(
                    session_id,
                    snapshot["seq"].as_u64().expect("seq"),
                    "say hello again",
                    protocol::TraceId::new(),
                )
                .is_err(),
            "a stale seq conflicts"
        );
        assert_eq!(
            event_kinds(&client, session_id)
                .iter()
                .filter(|kind| **kind == EventKind::HookDecided)
                .count(),
            2
        );
        // An unknown session: the kernel's error, and no hook runs for it.
        let runs = root.join("runs");
        std::fs::write(
            root.join("gate.sh"),
            format!(
                "echo run >> {}\necho '{{\"decision\":\"allow\"}}'\nexit 0\n",
                test_fixtures::sh_quote(&runs)
            ),
        )
        .expect("counting hook");
        assert!(
            connection
                .submit_prompt(protocol::SessionId::new(), 0, "x", protocol::TraceId::new())
                .is_err()
        );
        assert!(!runs.exists(), "no hook ran for an unknown session");
        let kinds = event_kinds(&client, session_id);
        let started = kinds
            .iter()
            .position(|kind| *kind == EventKind::TurnStarted)
            .expect("turn.started");
        let decided = kinds
            .iter()
            .position(|kind| *kind == EventKind::HookDecided)
            .expect("hook.decided");
        assert!(started < decided, "{kinds:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every `approval.resolved` payload in `session`, oldest first.
    fn resolutions(client: &InProcessKernelClient, session: SessionId) -> Vec<serde_json::Value> {
        use kernel::KernelClient as _;
        let tip = crate::approvals::client_call(client.get_session(session))
            .expect("session")
            .seq();
        (1..=tip)
            .filter_map(|seq| client.read_event(session, seq).ok())
            .filter(|event| event.kind() == EventKind::ApprovalResolved)
            .map(|event| event.payload().clone())
            .collect()
    }

    #[test]
    fn an_sdk_decision_resolves_the_pending_wait_once_by_its_token() {
        use kernel::KernelClient as _;
        let root = project_with_gate("daemon-approve", r#"{"decision":"allow"}"#);
        let (client, actor) = client_in(&root);
        let connection = Connection {
            client: client.clone(),
            actor: actor.clone(),
            root: root.clone(),
            trusted: true,
            daemon_token: None,
            mode_override: Default::default(),
        };
        let snapshot = connection
            .rpc("sessions.create", &serde_json::json!({}))
            .expect("session");
        let session: SessionId = snapshot["id"].as_str().expect("id").parse().expect("id");
        let resolve = |expected_seq: u64, decision: &str| {
            connection.rpc(
                "approvals.resolve",
                &serde_json::json!({
                    "session_id": session.to_string(),
                    "expected_seq": expected_seq,
                    "decision": decision,
                }),
            )
        };
        let tip = || {
            crate::approvals::client_call(client.get_session(session))
                .expect("session")
                .seq()
        };

        // Nothing pending: nothing to answer, nothing written.
        let err = resolve(tip(), "approved").expect_err("nothing pending");
        assert!(err.contains("no pending approval"), "{err}");
        assert!(resolutions(&client, session).is_empty());

        crate::approvals::client_call(client.record_approval(kernel::RecordApproval::new(
            session,
            tip(),
            actor,
            protocol::TraceId::new(),
            "wait-1",
            "call-1",
            "workspace_write",
            "write notes.txt",
        )))
        .expect("record approval");

        // A stale view of the session is refused before anything is written.
        let err = resolve(tip() - 1, "approved").expect_err("stale expected_seq");
        assert!(err.contains("moved past expected_seq"), "{err}");
        assert!(resolutions(&client, session).is_empty());

        // One resolution, by the wait's token. No turn was suspended here, so
        // the continuation cannot load — reported, not swallowed.
        let err = resolve(tip(), "approved").expect_err("no suspension to resume");
        assert!(err.contains("the decision was recorded"), "{err}");
        let resolved = resolutions(&client, session);
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert_eq!(resolved[0]["id"], "wait-1");
        assert_eq!(resolved[0]["wait_token"], "wait-1");
        assert_eq!(resolved[0]["decision"], "approved");

        // The wait is spent: a second answer has nothing to resolve.
        let err = resolve(tip(), "denied").expect_err("already answered");
        assert!(err.contains("no pending approval"), "{err}");
        assert_eq!(resolutions(&client, session).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_answer_is_attributed_to_its_resolver() {
        use kernel::KernelClient as _;
        let root = project_with_gate("daemon-answer", r#"{"decision":"allow"}"#);
        let (client, actor) = client_in(&root);
        let session = crate::approvals::client_call(client.create_session(
            kernel::CreateSession::new(ProjectId::new(), actor.clone(), protocol::TraceId::new()),
        ))
        .expect("session")
        .id();
        let tip = || {
            crate::approvals::client_call(client.get_session(session))
                .expect("session")
                .seq()
        };
        crate::approvals::client_call(client.record_approval(kernel::RecordApproval::new(
            session,
            tip(),
            actor.clone(),
            protocol::TraceId::new(),
            "wait-1",
            "call-1",
            "workspace_write",
            "write notes.txt",
        )))
        .expect("record approval");
        let before = tip();
        assert!(recorded_answer(&client, session, before, "wait-1").is_none());

        // Another resolver answers first: its decision and actor, not ours.
        let other = event_ledger::event::ActorRef::new(
            event_ledger::event::ActorKind::Human,
            &protocol::EventId::new().to_string(),
        )
        .expect("actor");
        crate::approvals::client_approve(
            &client,
            kernel::ResolveApproval::new(
                session,
                before,
                kernel::ApprovalDecision::Denied,
                other.clone(),
                protocol::TraceId::new(),
            )
            .with_wait_token("wait-1"),
        )
        .expect("resolve");
        let (decision, by) = recorded_answer(&client, session, before, "wait-1").expect("recorded");
        assert_eq!(decision, kernel::ApprovalDecision::Denied);
        assert_eq!(by, other);
        assert_ne!(by, actor);
        // Only resolutions after `from_seq` count, and only this token's.
        assert!(recorded_answer(&client, session, tip(), "wait-1").is_none());
        assert!(recorded_answer(&client, session, before, "wait-2").is_none());

        // What the SDK is told: ours, another resolver's (either field
        // differs), or — nothing recorded — the resolver's own error.
        let approved = kernel::ApprovalDecision::Approved;
        let denied = kernel::ApprovalDecision::Denied;
        let failed = || "boom".to_owned();
        assert!(
            resolve_failure(Some((denied, other.clone())), denied, &other, failed())
                .starts_with("the decision was recorded")
        );
        for recorded in [(denied, actor.clone()), (approved, other.clone())] {
            assert!(
                resolve_failure(Some(recorded), denied, &other, failed())
                    .starts_with("the approval was answered by another resolver first")
            );
        }
        assert_eq!(resolve_failure(None, denied, &other, failed()), "boom");
        let _ = std::fs::remove_dir_all(&root);
    }
}
