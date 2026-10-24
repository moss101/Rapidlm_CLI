//! Observer hook stages (EXT-03 §3.4): `permission_requested`,
//! `permission_resolved` and `notification`.
//!
//! These stages cannot block, rewrite or add context, and they fail open: a
//! defect in the hook is a recorded `hook.failed`, never an effect on the
//! session. They also never delay the thing they observe. An event is not
//! run where it happens but handed to one bounded queue and run on a worker
//! thread, so a slow hook cannot hold up creating an approval, resolving
//! one or finishing a turn. Order is the order events were emitted. The
//! queue holds [`QUEUE_CAPACITY`] events; when it is full the oldest is
//! dropped and a `hook.dropped` record says how many, at most once per
//! [`DROPPED_RECORD_INTERVAL`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::exec_tools::HookEvents;
use crate::hooks::{HOOK_TIMEOUT, HookEntry, HooksConfig};

/// Events the queue holds before it drops the oldest.
pub const QUEUE_CAPACITY: usize = 32;
/// Least time between two `hook.dropped` records.
pub const DROPPED_RECORD_INTERVAL: Duration = Duration::from_secs(60);

/// Bytes of a free-text payload field (`summary`, `title`).
const MAX_TEXT_BYTES: usize = 512;

/// The three observer stages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverStage {
    PermissionRequested,
    PermissionResolved,
    Notification,
}

impl ObserverStage {
    pub const fn name(self) -> &'static str {
        match self {
            Self::PermissionRequested => "permission_requested",
            Self::PermissionResolved => "permission_resolved",
            Self::Notification => "notification",
        }
    }

    fn entries(self, hooks: &HooksConfig) -> &[HookEntry] {
        match self {
            Self::PermissionRequested => &hooks.permission_requested,
            Self::PermissionResolved => &hooks.permission_resolved,
            Self::Notification => &hooks.notification,
        }
    }
}

/// The attention kinds a `notification` hook can match on (a closed set).
pub const NOTIFICATION_KINDS: [&str; 6] = [
    "turn_complete",
    "agent_finished",
    "job_finished",
    "loop_result",
    "monitor_flooded",
    "goal_changed",
];

/// One queued event: everything a worker needs, so no live state is shared
/// with the emitting thread.
pub struct ObserverJob {
    stage: &'static str,
    entries: Vec<HookEntry>,
    subject: Option<String>,
    input: String,
    events: Option<Arc<dyn HookEvents>>,
}

type Runner = Arc<dyn Fn(ObserverJob) + Send + Sync>;

struct QueueState {
    jobs: VecDeque<ObserverJob>,
    /// Events dropped since the last `hook.dropped` record, by stage.
    pending_dropped: Vec<(&'static str, u64)>,
    last_record: Option<Instant>,
    running: bool,
}

/// A bounded FIFO with one worker thread, started on first use.
pub struct ObserverQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    idle: Condvar,
    capacity: usize,
    interval: Duration,
    runner: Runner,
    started: AtomicBool,
}

impl ObserverQueue {
    pub fn new(capacity: usize, interval: Duration, runner: Runner) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                jobs: VecDeque::new(),
                pending_dropped: Vec::new(),
                last_record: None,
                running: false,
            }),
            ready: Condvar::new(),
            idle: Condvar::new(),
            capacity: capacity.max(1),
            interval,
            runner,
            started: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue `job`, dropping the oldest event if the queue is full. Never
    /// waits for a hook.
    pub fn submit(self: &Arc<Self>, job: ObserverJob) {
        self.ensure_worker();
        let mut dropped_to_record: Vec<(&'static str, u64)> = Vec::new();
        let events = job.events.clone();
        {
            let mut state = self.lock();
            if state.jobs.len() >= self.capacity
                && let Some(dropped) = state.jobs.pop_front()
            {
                match state
                    .pending_dropped
                    .iter_mut()
                    .find(|(stage, _)| *stage == dropped.stage)
                {
                    Some((_, count)) => *count += 1,
                    None => state.pending_dropped.push((dropped.stage, 1)),
                }
                if state
                    .last_record
                    .is_none_or(|at| at.elapsed() >= self.interval)
                {
                    state.last_record = Some(Instant::now());
                    dropped_to_record = std::mem::take(&mut state.pending_dropped);
                }
            }
            state.jobs.push_back(job);
        }
        self.ready.notify_one();
        if let Some(events) = events {
            for (stage, count) in dropped_to_record {
                events.dropped(stage, count);
            }
        }
    }

    fn ensure_worker(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = Arc::clone(self);
        std::thread::Builder::new()
            .name("rapid-observer-hooks".to_owned())
            .spawn(move || queue.work())
            .ok();
    }

    fn work(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock();
                loop {
                    if let Some(job) = state.jobs.pop_front() {
                        state.running = true;
                        break job;
                    }
                    state.running = false;
                    self.idle.notify_all();
                    state = self
                        .ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let events = job.events.clone();
            (self.runner)(job);
            // A count that has waited out the interval without another drop to
            // carry it is reported when the worker next finishes an event.
            let due = {
                let mut state = self.lock();
                let due = !state.pending_dropped.is_empty()
                    && state
                        .last_record
                        .is_none_or(|at| at.elapsed() >= self.interval);
                if due {
                    state.last_record = Some(Instant::now());
                    Some(std::mem::take(&mut state.pending_dropped))
                } else {
                    None
                }
            };
            if let (Some(due), Some(events)) = (due, events) {
                for (stage, count) in due {
                    events.dropped(stage, count);
                }
            }
        }
    }

    /// Wait until every event queued so far has run (tests, and process exit).
    pub fn drain(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut state = self.lock();
        while !state.jobs.is_empty() || state.running {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .idle
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() && (!state.jobs.is_empty() || state.running) {
                return false;
            }
        }
        true
    }

    /// Events waiting to run.
    pub fn queued(&self) -> usize {
        self.lock().jobs.len()
    }
}

/// The process-wide queue the real stages use.
fn global() -> &'static Arc<ObserverQueue> {
    static QUEUE: OnceLock<Arc<ObserverQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        ObserverQueue::new(QUEUE_CAPACITY, DROPPED_RECORD_INTERVAL, Arc::new(run_job))
    })
}

/// Run one observer event: its hooks, in order; a hook that fails is a
/// recorded `hook.failed`. Whatever the hooks print or decide is ignored.
fn run_job(job: ObserverJob) {
    let report = crate::hooks::run_observer_hooks(
        job.stage,
        &job.entries,
        job.subject.as_deref(),
        &job.input,
        HOOK_TIMEOUT,
    );
    if let Some(events) = &job.events {
        for failure in &report.failures {
            events.failed("", "", failure);
        }
    }
}

/// Wait for the real queue to run what it holds (process exit, tests).
pub fn drain(limit: Duration) -> bool {
    global().drain(limit)
}

/// Whether any observer hook is configured for `stage`.
pub fn is_configured(hooks: &HooksConfig, stage: ObserverStage) -> bool {
    !stage.entries(hooks).is_empty()
}

/// Queue an event for `stage` on the real queue. A stage with no hooks, or
/// whose hooks' matchers all pass over `subject`, does nothing at all.
pub(crate) fn emit(
    hooks: &HooksConfig,
    events: Option<Arc<dyn HookEvents>>,
    stage: ObserverStage,
    subject: Option<&str>,
    payload: serde_json::Value,
) {
    emit_on(global(), hooks, events, stage, subject, payload);
}

/// [`emit`] on a queue the caller chose.
pub(crate) fn emit_on(
    queue: &Arc<ObserverQueue>,
    hooks: &HooksConfig,
    events: Option<Arc<dyn HookEvents>>,
    stage: ObserverStage,
    subject: Option<&str>,
    payload: serde_json::Value,
) {
    use crate::hooks::HookSpec;
    let entries = stage.entries(hooks);
    if entries.is_empty() || !entries.iter().any(|entry| entry.applies_to(subject)) {
        return;
    }
    let mut payload = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("payload".to_owned(), other);
            map
        }
    };
    payload.insert("event".to_owned(), stage.name().into());
    // The recorder knows its session; the thread may not (a slash command, a
    // worker). The thread's run identity adds the turn, and the session when
    // the recorder has none.
    if let Some(session) = events.as_ref().and_then(|events| events.session()) {
        payload
            .entry("session_id")
            .or_insert_with(|| session.into());
    }
    if let Some(identity) = crate::run_identity::current() {
        payload
            .entry("session_id")
            .or_insert_with(|| identity.session.clone().into());
        if let Some(turn) = identity.turn {
            payload.entry("turn_id").or_insert_with(|| turn.into());
        }
    }
    queue.submit(ObserverJob {
        stage: stage.name(),
        entries: entries.to_vec(),
        subject: subject.map(str::to_owned),
        input: serde_json::Value::Object(payload).to_string(),
        events,
    });
}

/// `text` cut to [`MAX_TEXT_BYTES`] on a character boundary, control
/// characters flattened to spaces (a hook reads this as a line of JSON text,
/// not as terminal output).
pub fn bounded_text(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let mut end = flattened.len().min(MAX_TEXT_BYTES);
    while !flattened.is_char_boundary(end) {
        end -= 1;
    }
    flattened[..end].to_owned()
}

/// What an approval request looked like, for `permission_requested`.
pub struct RequestedApproval<'a> {
    pub approval_id: &'a str,
    pub tool: &'a str,
    pub summary: &'a str,
    /// `ApprovalRequest::source` as recorded (`hook:pre_tool_use[0]#digest`,
    /// `plan:<id>#r<n>`, `mcp:<server>`), or `None` for a lattice ask.
    pub source: Option<&'a str>,
    pub arguments_digest: Option<&'a str>,
    /// What an ask with no source of its own is, on the surface that raised
    /// it: `clarification` or `workflow` for a human wait, `lattice` otherwise.
    pub default_source: &'a str,
}

/// The `source` token the payload carries: `lattice`, `hook:<position>`,
/// `plan`, `mcp:<server>`, `clarification` or `workflow`.
pub fn source_label(request: &RequestedApproval<'_>) -> String {
    match request.source {
        Some(source) if source.starts_with("hook:") => {
            let position = source
                .trim_start_matches("hook:")
                .split('#')
                .next()
                .unwrap_or_default();
            format!("hook:{position}")
        }
        Some(source) if source.starts_with("plan:") => "plan".to_owned(),
        Some(source) if source.starts_with("mcp:") => bounded_text(source),
        Some(_) => "lattice".to_owned(),
        None if request.tool.starts_with("workflow:") => "workflow".to_owned(),
        None => request.default_source.to_owned(),
    }
}

/// Fire `permission_requested`.
pub(crate) fn permission_requested(
    hooks: &HooksConfig,
    events: Option<Arc<dyn HookEvents>>,
    request: &RequestedApproval<'_>,
) {
    emit(
        hooks,
        events,
        ObserverStage::PermissionRequested,
        Some(request.tool),
        serde_json::json!({
            "approval_id": request.approval_id,
            "tool": request.tool,
            "summary": bounded_text(request.summary),
            "source": source_label(request),
            "arguments_digest": request.arguments_digest,
        }),
    );
}

/// The surfaces that resolve an approval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveSurface {
    Tui,
    Acp,
    Daemon,
    Workflow,
}

impl ResolveSurface {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Acp => "acp",
            Self::Daemon => "daemon",
            Self::Workflow => "workflow",
        }
    }
}

/// Fire `permission_resolved`.
pub(crate) fn permission_resolved(
    hooks: &HooksConfig,
    events: Option<Arc<dyn HookEvents>>,
    approval_id: &str,
    tool: &str,
    approved: bool,
    remember: bool,
    surface: ResolveSurface,
) {
    emit(
        hooks,
        events,
        ObserverStage::PermissionResolved,
        Some(tool),
        serde_json::json!({
            "approval_id": approval_id,
            "tool": tool,
            "decision": if approved { "approved" } else { "denied" },
            "remember": remember,
            "surface": surface.name(),
        }),
    );
}

/// Where an asynchronous event source (a job table, a child's lifecycle, a
/// loop) finds the hooks to run when something it watches ends: the project's
/// settings are read when the event happens, not when the source was built, so
/// a hook added mid-session is honoured. Hooks are project settings, so
/// nothing is read for an untrusted project.
#[derive(Clone)]
pub(crate) struct ObserverSource {
    root: std::path::PathBuf,
    trusted: bool,
    events: Arc<dyn HookEvents>,
}

impl ObserverSource {
    pub(crate) fn new(root: &std::path::Path, trusted: bool, events: Arc<dyn HookEvents>) -> Self {
        Self {
            root: root.to_path_buf(),
            trusted,
            events,
        }
    }

    /// Fire a `notification` for this project, if a hook wants it.
    pub(crate) fn notify(&self, notice: &Notice<'_>) {
        if !self.trusted {
            return;
        }
        let hooks = crate::interactive::load_project_integrations(&self.root).hooks;
        if !is_configured(&hooks, ObserverStage::Notification) {
            return;
        }
        notification(&hooks, Some(Arc::clone(&self.events)), notice);
    }
}

/// What a `notification` hook is told.
pub struct Notice<'a> {
    /// One of [`NOTIFICATION_KINDS`].
    pub kind: &'a str,
    pub title: &'a str,
    /// A short token (`ok`, `failed`, `cancelled`, …) where the source has one.
    pub outcome: Option<&'a str>,
    pub job_id: Option<&'a str>,
    pub loop_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
}

/// Fire `notification`.
pub(crate) fn notification(
    hooks: &HooksConfig,
    events: Option<Arc<dyn HookEvents>>,
    notice: &Notice<'_>,
) {
    debug_assert!(
        NOTIFICATION_KINDS.contains(&notice.kind),
        "{} is not a notification kind",
        notice.kind
    );
    emit(
        hooks,
        events,
        ObserverStage::Notification,
        Some(notice.kind),
        serde_json::json!({
            "kind": notice.kind,
            "title": bounded_text(notice.title),
            "outcome": notice.outcome,
            "job_id": notice.job_id,
            "loop_id": notice.loop_id,
            "agent_id": notice.agent_id,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookMatcher, entries};
    use std::sync::mpsc;

    /// A recorder that remembers what it was told.
    #[derive(Default)]
    struct Recording {
        failed: Mutex<Vec<(String, String)>>,
        dropped: Mutex<Vec<(String, u64)>>,
    }

    impl HookEvents for Recording {
        fn decided(&self, _: &str, _: &str, _: &crate::hooks::HookDecisionRecord) {}
        fn rewritten(&self, _: &str, _: &str, _: &crate::exec_tools::HookRewriteRecord) {}
        fn failed(&self, _: &str, _: &str, failure: &crate::hooks::HookFailure) {
            self.failed
                .lock()
                .expect("failed")
                .push((failure.event.to_owned(), failure.hook.clone()));
        }
        fn dropped(&self, stage: &str, count: u64) {
            self.dropped
                .lock()
                .expect("dropped")
                .push((stage.to_owned(), count));
        }
    }

    fn config_with(stage: ObserverStage, hook: HookEntry) -> HooksConfig {
        let mut hooks = HooksConfig::default();
        match stage {
            ObserverStage::PermissionRequested => hooks.permission_requested.push(hook),
            ObserverStage::PermissionResolved => hooks.permission_resolved.push(hook),
            ObserverStage::Notification => hooks.notification.push(hook),
        }
        hooks
    }

    /// A queue whose runner records each event's `n` and blocks on the first
    /// until `release` is sent.
    fn held_queue(
        capacity: usize,
        interval: Duration,
    ) -> (Arc<ObserverQueue>, mpsc::Sender<()>, Arc<Mutex<Vec<u64>>>) {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let first = std::sync::atomic::AtomicBool::new(true);
        let runner: Runner = Arc::new(move |job: ObserverJob| {
            let n = serde_json::from_str::<serde_json::Value>(&job.input)
                .ok()
                .and_then(|value| value["n"].as_u64())
                .unwrap_or(u64::MAX);
            if first.swap(false, Ordering::AcqRel) {
                let _ = gate.lock().expect("gate").recv();
            }
            record.lock().expect("seen").push(n);
        });
        (
            ObserverQueue::new(capacity, interval, runner),
            release,
            seen,
        )
    }

    fn notify(
        queue: &Arc<ObserverQueue>,
        hooks: &HooksConfig,
        events: Option<Arc<dyn HookEvents>>,
        n: u64,
    ) {
        emit_on(
            queue,
            hooks,
            events,
            ObserverStage::Notification,
            Some("turn_complete"),
            serde_json::json!({ "n": n }),
        );
    }

    #[test]
    fn a_slow_observer_does_not_delay_the_emitter_and_order_is_kept() {
        let (queue, release, seen) = held_queue(8, Duration::from_secs(60));
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        for n in 1..=4 {
            notify(&queue, &hooks, None, n);
        }
        // Every emit has returned while the first event is still being "run"
        // (blocked on the gate): the emitter never waited for a hook.
        assert!(
            seen.lock().expect("seen").is_empty(),
            "nothing has finished, yet all four emits returned"
        );
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [1, 2, 3, 4], "emission order");
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_records_how_many() {
        let (queue, release, seen) = held_queue(3, Duration::ZERO);
        let events = Arc::new(Recording::default());
        let shared: Arc<dyn HookEvents> = events.clone();
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, Some(shared.clone()), 1);
        // Wait for the worker to take event 1 (it then blocks on the gate), so
        // the queue is empty and the next six fill it past its capacity of 3.
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for n in 2..=7 {
            notify(&queue, &hooks, Some(shared.clone()), n);
        }
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(
            *seen.lock().expect("seen"),
            [1, 5, 6, 7],
            "events 2, 3 and 4 were the oldest when the queue was full"
        );
        let total: u64 = events
            .dropped
            .lock()
            .expect("dropped")
            .iter()
            .map(|(_, c)| c)
            .sum();
        assert_eq!(total, 3, "{:?}", events.dropped.lock().expect("dropped"));
        assert!(
            events
                .dropped
                .lock()
                .expect("dropped")
                .iter()
                .all(|(stage, _)| stage == "notification")
        );
    }

    #[test]
    fn drops_are_recorded_at_most_once_per_interval() {
        let (queue, release, _seen) = held_queue(1, Duration::from_secs(3600));
        let events = Arc::new(Recording::default());
        let shared: Arc<dyn HookEvents> = events.clone();
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, Some(shared.clone()), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for n in 2..=20 {
            notify(&queue, &hooks, Some(shared.clone()), n);
        }
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        let records = events.dropped.lock().expect("dropped").clone();
        assert_eq!(
            records.len(),
            1,
            "an hour-long interval allows one record, however many events were dropped: {records:?}"
        );
        assert_eq!(
            records[0].1, 1,
            "the first record carries what had been dropped by then"
        );
    }

    #[test]
    fn a_stage_with_no_selected_hook_queues_nothing() {
        let (queue, release, seen) = held_queue(8, Duration::from_secs(60));
        let none = HooksConfig::default();
        notify(&queue, &none, None, 1);
        let selective = HookEntry {
            matcher: Some(HookMatcher::new("job_finished").expect("matcher")),
            ..HookEntry::plain("true")
        };
        let hooks = config_with(ObserverStage::Notification, selective);
        notify(&queue, &hooks, None, 2);
        assert_eq!(queue.queued(), 0, "no hook applies to turn_complete");
        drop(release);
        assert!(seen.lock().expect("seen").is_empty());
    }

    #[test]
    fn the_hook_receives_the_documented_payload_and_a_failure_is_recorded() {
        let dir = std::env::temp_dir().join(format!("observer-payload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("stdin.json");
        let path = dir.join("hook.sh");
        std::fs::write(
            &path,
            format!("cat > {}\nexit 3\n", test_fixtures::sh_quote(&capture)),
        )
        .expect("script");
        let command = format!("sh {}", test_fixtures::slash_path(&path));
        let hooks = config_with(
            ObserverStage::PermissionRequested,
            HookEntry::plain(command),
        );
        let events = Arc::new(Recording::default());
        let shared: Arc<dyn HookEvents> = events.clone();
        let identity = crate::run_identity::RunIdentity {
            session: "sess-1".to_owned(),
            turn: Some("turn-9".to_owned()),
        };
        crate::run_identity::scoped(Some(&identity), || {
            permission_requested(
                &hooks,
                Some(shared),
                &RequestedApproval {
                    approval_id: "tok-1",
                    tool: "shell_exec",
                    summary: "run\u{7}it\nnow",
                    source: Some("hook:pre_tool_use[0]#abc123"),
                    arguments_digest: Some("deadbeef"),
                    default_source: "lattice",
                },
            );
        });
        assert!(drain(Duration::from_secs(10)));
        let got: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).expect("stdin")).expect("json");
        assert_eq!(got["event"], "permission_requested");
        assert_eq!(got["approval_id"], "tok-1");
        assert_eq!(got["tool"], "shell_exec");
        assert_eq!(
            got["summary"], "run it now",
            "control characters are flattened"
        );
        assert_eq!(got["source"], "hook:pre_tool_use[0]");
        assert_eq!(got["arguments_digest"], "deadbeef");
        assert_eq!(got["session_id"], "sess-1");
        assert_eq!(got["turn_id"], "turn-9");
        assert_eq!(
            events.failed.lock().expect("failed").clone(),
            [(
                "permission_requested".to_owned(),
                "permission_requested[0]".to_owned()
            )],
            "an observer that exits non-zero is a recorded failure, never an effect"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_source_label_is_one_of_the_documented_tokens() {
        let label = |tool: &str, source: Option<&str>, default: &str| {
            source_label(&RequestedApproval {
                approval_id: "t",
                tool,
                summary: "",
                source,
                arguments_digest: None,
                default_source: default,
            })
        };
        assert_eq!(label("workspace_write", None, "lattice"), "lattice");
        assert_eq!(
            label("shell_exec", Some("hook:pre_tool_use[2]#9f"), "lattice"),
            "hook:pre_tool_use[2]"
        );
        assert_eq!(label("plan_exit", Some("plan:p1#r2"), "lattice"), "plan");
        assert_eq!(
            label("mcp__srv__tool", Some("mcp:srv"), "lattice"),
            "mcp:srv"
        );
        assert_eq!(label("ask_user", None, "clarification"), "clarification");
        assert_eq!(label("ask_user", None, "workflow"), "workflow");
        assert_eq!(label("workflow:approval", None, "lattice"), "workflow");
    }

    #[test]
    fn free_text_is_cut_on_a_character_boundary() {
        let text = "é".repeat(400);
        let cut = bounded_text(&text);
        assert!(cut.len() <= MAX_TEXT_BYTES && cut.chars().all(|c| c == 'é'));
        assert_eq!(bounded_text("a\tb"), "a b");
        assert!(NOTIFICATION_KINDS.contains(&"goal_changed"));
        assert_eq!(entries(["x"]).len(), 1);
    }
}
