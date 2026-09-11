//! Crash-recovery integration: kill a child RapidLM fixture mid-turn.
//!
//! The child commits a durable `tool.started` against a file-backed ledger,
//! then hangs with that turn still open. The harness uses a real process kill
//! (`Child::kill` / SIGKILL), not graceful shutdown, and a second process
//! reopens the same DB. Recovery must interrupt the turn, park the goal via
//! [`kernel::park_recovered_goal`], and must not start autonomous work.

use std::fs;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use event_ledger::event::{ActorKind, ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger};
use kernel::{
    CancellationToken, CreateSession, GoalState, GoalStopReason, InProcessKernelClient,
    KernelClient, RecoveryAction, RecoveryManager, SessionStatus, SubmitTurn, park_recovered_goal,
    recover_session,
};
use protocol::{EventId, GoalId, ProjectId, RedactionClass, SessionId, TraceId};
use serde_json::{Value, json};

const ROLE_ENV: &str = "RLM_CRASH_RECOVERY_ROLE";
const LEDGER_ENV: &str = "RLM_CRASH_RECOVERY_LEDGER";
const SESSION_ENV: &str = "RLM_CRASH_RECOVERY_SESSION";
const RESULT_ENV: &str = "RLM_CRASH_RECOVERY_RESULT";

const ROLE_MID_TURN: &str = "mid-turn";
const ROLE_RECOVER: &str = "recover";

const TEST_BIN_FILTER: &str = "kill_mid_turn_then_restart_same_db";
const TOOL_CALL_ID: &str = "call_crash_1";
const WAIT_BUDGET: Duration = Duration::from_secs(20);
const POLL_EVERY: Duration = Duration::from_millis(20);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[test]
fn kill_mid_turn_then_restart_same_db() {
    match fixture_role() {
        Some(role) if role == ROLE_MID_TURN => run_mid_turn_fixture(),
        Some(role) if role == ROLE_RECOVER => run_recover_fixture(),
        Some(role) => panic!("unknown {ROLE_ENV}={role}"),
        None => run_harness(),
    }
}

fn run_harness() {
    let tmp = TempDb::create();
    let session_path = tmp.dir.join("session.json");
    let result_path = tmp.dir.join("recovery.json");

    let mut fixture = spawn_role(ROLE_MID_TURN, &tmp.db, &session_path, &result_path);
    let meta = wait_for_durable_tool_started(&tmp.db, &session_path);
    let kinds_before_kill = event_kinds(&tmp.db, meta.session_id);
    assert_eq!(
        kinds_before_kill.last().copied(),
        Some(EventKind::ToolStarted),
        "kill must happen after durable tool.started: {kinds_before_kill:?}"
    );
    assert!(
        !kinds_before_kill.contains(&EventKind::SessionRecovered),
        "fixture must not recover itself before kill: {kinds_before_kill:?}"
    );

    let killed = fixture.kill_ungracefully();
    assert!(
        !killed.success(),
        "real process kill must not look like a clean exit: {killed:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            killed.signal(),
            Some(9),
            "fixture must die from SIGKILL, not a handled shutdown: {killed:?}"
        );
    }

    let kinds_after_kill = event_kinds(&tmp.db, meta.session_id);
    assert_eq!(
        kinds_after_kill, kinds_before_kill,
        "ungraceful kill must not append recovery or completion events"
    );

    let mut restarted = spawn_role(ROLE_RECOVER, &tmp.db, &session_path, &result_path);
    let recovered_status = restarted.wait_success();
    assert!(
        recovered_status.success(),
        "restart process must exit after recovery without launching work: {recovered_status:?}"
    );

    let result = read_json(&result_path);
    assert_eq!(result["status"].as_str(), Some("paused"));
    assert_eq!(result["goal_state"].as_str(), Some("paused"));
    assert_eq!(result["stop_reason"].as_str(), Some("process_recovered"));
    assert!(result["active_turn"].is_null());

    let kinds = event_kinds(&tmp.db, meta.session_id);
    assert_eq!(
        kinds,
        vec![
            EventKind::SessionCreated,
            EventKind::GoalCreated,
            EventKind::TurnStarted,
            EventKind::ToolRequested,
            EventKind::ToolAuthorized,
            EventKind::ToolStarted,
            EventKind::SessionRecovered,
            EventKind::ToolFailed,
            EventKind::TurnInterrupted,
            EventKind::GoalPaused,
        ]
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == EventKind::TurnStarted)
            .count(),
        1,
        "restart must not submit a new turn: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|kind| matches!(
            kind,
            EventKind::ToolCompleted
                | EventKind::TurnCompleted
                | EventKind::GoalResumed
                | EventKind::GoalCompleted
        )),
        "recovery must not fabricate success or resume autonomy: {kinds:?}"
    );

    let failed = load_event(&tmp.db, meta.session_id, 8);
    assert_eq!(failed.kind(), EventKind::ToolFailed);
    assert_eq!(
        failed.payload().get("call_id").and_then(Value::as_str),
        Some(TOOL_CALL_ID)
    );
    assert_eq!(
        failed.payload().get("reason").and_then(Value::as_str),
        Some(GoalStopReason::ProcessRecovered.as_str())
    );

    let paused = load_event(&tmp.db, meta.session_id, 10);
    assert_eq!(paused.kind(), EventKind::GoalPaused);
    assert_eq!(
        paused
            .payload()
            .get("process_recovered")
            .and_then(Value::as_bool),
        Some(true)
    );

    let manager = RecoveryManager::open(&tmp.db).expect("reopen recovered ledger");
    let snapshot = recover_session(&manager, meta.session_id, &CancellationToken::new())
        .expect("idempotent recover");
    assert_eq!(snapshot.status(), SessionStatus::Paused);
    assert!(snapshot.active_turn().is_none());
    let goal = snapshot.top_level_goal().expect("parked goal");
    assert_eq!(goal.id(), meta.goal_id);
    assert_eq!(goal.state(), GoalState::Paused);
    assert_eq!(goal.stop_reason(), Some(GoalStopReason::ProcessRecovered));
    assert_eq!(
        park_recovered_goal(meta.goal_id),
        RecoveryAction::PauseGoal {
            goal_id: meta.goal_id,
            reason: GoalStopReason::ProcessRecovered,
        }
    );
    assert_eq!(
        manager
            .ledger()
            .last_seq(meta.session_id, &ledger_live())
            .expect("last_seq after second recover"),
        10,
        "repeated recovery must not append again"
    );
}

fn run_mid_turn_fixture() {
    let db = ledger_path_from_env();
    let session_path = path_from_env(SESSION_ENV);
    let client = InProcessKernelClient::open(&db).expect("open fixture kernel");
    let created = block_on(client.create_session(create_req())).expect("create session");
    let ledger = EventLedger::open(&db).expect("open fixture ledger");
    let goal_id = GoalId::new();
    append(
        &ledger,
        created.id(),
        EventKind::GoalCreated,
        json!({"goal_id": goal_id, "statement": "finish the interrupted turn"}),
    );
    let handle = block_on(client.submit_turn(SubmitTurn::new(
        created.id(),
        2,
        actor(),
        TraceId::new(),
        "hello",
    )))
    .expect("submit turn");
    append(
        &ledger,
        created.id(),
        EventKind::ToolRequested,
        json!({"call_id": TOOL_CALL_ID}),
    );
    append(
        &ledger,
        created.id(),
        EventKind::ToolAuthorized,
        json!({"call_id": TOOL_CALL_ID}),
    );
    let started = append(
        &ledger,
        created.id(),
        EventKind::ToolStarted,
        json!({"call_id": TOOL_CALL_ID}),
    );
    write_json_atomic(
        &session_path,
        &json!({
            "session_id": created.id(),
            "goal_id": goal_id,
            "turn_id": handle.turn_id(),
            "call_id": TOOL_CALL_ID,
            "tool_started_seq": started.seq(),
        }),
    );
    emit_tool_started_jsonl(created.id(), started.seq());
    hang_mid_turn();
}

fn run_recover_fixture() {
    let db = ledger_path_from_env();
    let session_path = path_from_env(SESSION_ENV);
    let result_path = path_from_env(RESULT_ENV);
    let meta = SessionMeta::from_value(&read_json(&session_path));
    let manager = RecoveryManager::open(&db).expect("restart RecoveryManager");
    let snapshot = recover_session(&manager, meta.session_id, &CancellationToken::new())
        .expect("recover after crash");
    let goal = snapshot.top_level_goal().expect("goal after recover");
    write_json_atomic(
        &result_path,
        &json!({
            "status": snapshot.status().as_str(),
            "goal_state": goal.state().as_str(),
            "stop_reason": goal.stop_reason().map(|reason| reason.as_str()),
            "active_turn": snapshot.active_turn().map(|id| id.to_string()),
            "seq": snapshot.seq(),
        }),
    );
}

fn hang_mid_turn() -> ! {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn emit_tool_started_jsonl(session_id: SessionId, seq: u64) {
    let line = serde_json::to_string(&json!({
        "schema": 1,
        "type": "tool.started",
        "session_id": session_id,
        "seq": seq,
        "data": {"call_id": TOOL_CALL_ID},
    }))
    .expect("jsonl");
    let mut out = io::stdout().lock();
    writeln!(out, "{line}").expect("write jsonl");
    out.flush().expect("flush jsonl");
}

fn wait_for_durable_tool_started(db: &Path, session_path: &Path) -> SessionMeta {
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for durable tool.started in {}",
                db.display()
            );
        }
        if session_path.exists() {
            let meta = SessionMeta::from_value(&read_json(session_path));
            if let Ok(event) = EventLedger::open(db).and_then(|ledger| {
                ledger.get(meta.session_id, meta.tool_started_seq, &ledger_live())
            }) && event.kind() == EventKind::ToolStarted
            {
                assert_eq!(
                    event.payload().get("call_id").and_then(Value::as_str),
                    Some(TOOL_CALL_ID)
                );
                return meta;
            }
        }
        thread::sleep(POLL_EVERY);
    }
}

fn spawn_role(role: &str, db: &Path, session_path: &Path, result_path: &Path) -> FixtureChild {
    let exe = std::env::current_exe().expect("test binary");
    let child = Command::new(exe)
        .arg(TEST_BIN_FILTER)
        .arg("--exact")
        .arg("--nocapture")
        .env(ROLE_ENV, role)
        .env(LEDGER_ENV, db)
        .env(SESSION_ENV, session_path)
        .env(RESULT_ENV, result_path)
        .env("RUST_TEST_THREADS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn RapidLM fixture");
    FixtureChild { child: Some(child) }
}

fn event_kinds(db: &Path, session_id: SessionId) -> Vec<EventKind> {
    let ledger = EventLedger::open(db).expect("open ledger");
    let last = ledger
        .last_seq(session_id, &ledger_live())
        .expect("last_seq");
    (1..=last)
        .map(|seq| {
            ledger
                .get(session_id, seq, &ledger_live())
                .expect("get event")
                .kind()
        })
        .collect()
}

fn load_event(
    db: &Path,
    session_id: SessionId,
    seq: u64,
) -> event_ledger::event::ErasedEventEnvelope {
    EventLedger::open(db)
        .expect("open ledger")
        .get(session_id, seq, &ledger_live())
        .expect("get event")
}

fn append(
    ledger: &EventLedger,
    session: SessionId,
    kind: EventKind,
    payload: Value,
) -> event_ledger::event::ErasedEventEnvelope {
    ledger
        .append(
            session,
            actor(),
            kind,
            payload,
            &append_options(),
            &ledger_live(),
        )
        .expect("append")
        .erase()
        .expect("erase")
}

fn append_options() -> AppendOptions {
    AppendOptions {
        redaction: RedactionClass::Project,
        trace_id: TraceId::new(),
        expected_seq: None,
    }
}

fn create_req() -> CreateSession {
    CreateSession::new(ProjectId::new(), actor(), TraceId::new())
}

fn actor() -> ActorRef {
    ActorRef::new(ActorKind::Human, &EventId::new().to_string()).expect("actor")
}

fn ledger_live() -> event_ledger::ledger::CancellationToken {
    event_ledger::ledger::CancellationToken::new()
}

fn fixture_role() -> Option<String> {
    match std::env::var(ROLE_ENV) {
        Ok(role) if !role.is_empty() => Some(role),
        _ => None,
    }
}

fn ledger_path_from_env() -> PathBuf {
    path_from_env(LEDGER_ENV)
}

fn path_from_env(key: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key} missing")))
}

fn write_json_atomic(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value).expect("encode")).expect("write tmp");
    fs::rename(&tmp, path).expect("publish json");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read json")).expect("decode json")
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("in-process kernel future stayed pending"),
    }
}

struct SessionMeta {
    session_id: SessionId,
    goal_id: GoalId,
    tool_started_seq: u64,
}

impl SessionMeta {
    fn from_value(value: &Value) -> Self {
        Self {
            session_id: parse_id(value, "session_id"),
            goal_id: parse_id(value, "goal_id"),
            tool_started_seq: value
                .get("tool_started_seq")
                .and_then(Value::as_u64)
                .expect("tool_started_seq"),
        }
    }
}

fn parse_id<T: std::str::FromStr>(value: &Value, field: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{field} missing"))
        .parse()
        .unwrap_or_else(|err| panic!("{field}: {err:?}"))
}

struct FixtureChild {
    child: Option<Child>,
}

impl FixtureChild {
    fn kill_ungracefully(&mut self) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("fixture still running");
        child.kill().expect("SIGKILL / TerminateProcess");
        child.wait().expect("wait after kill")
    }

    fn wait_success(&mut self) -> std::process::ExitStatus {
        let child = self.child.take().expect("restart process");
        let output = child.wait_with_output().expect("wait for restart");
        if !output.status.success() {
            panic!(
                "recover fixture failed ({:?})\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output.status
    }
}

impl Drop for FixtureChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TempDb {
    dir: PathBuf,
    db: PathBuf,
}

impl TempDb {
    fn create() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-crash-recovery-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let db = dir.join("ledger.sqlite");
        Self { dir, db }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
