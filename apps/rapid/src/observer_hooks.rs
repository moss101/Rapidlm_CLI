//! Observer hook stages (EXT-03 §3.4): `permission_requested`,
//! `permission_resolved` and `notification`.
//!
//! These stages cannot block, rewrite or add context, and they fail open: a
//! defect in the hook is a recorded `hook.failed`, never an effect on the
//! session. They also never delay the thing they observe. An event is not
//! run where it happens but handed to one bounded queue and run on a worker
//! thread, so a slow hook cannot hold up creating an approval, resolving
//! one or finishing a turn. Order is the order events were emitted, except
//! that a waiting permission event runs ahead of a waiting notification. The
//! queue holds [`QUEUE_CAPACITY`] events; when it is full the oldest
//! notification is dropped (a permission event is dropped only to make room
//! for another), and `hook.dropped` records how many, per session, at most once
//! per [`DROPPED_RECORD_INTERVAL`] while the run goes on and in full when it
//! ends.

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

/// Bytes of an approval request's `summary`.
const MAX_SUMMARY_BYTES: usize = 512;
/// Bytes of a notification's `title`.
const MAX_TITLE_BYTES: usize = 200;

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
    /// The run the event belongs to. The worker is not that run's thread, so
    /// the hooks are given its session and turn ids from here, as every other
    /// hook is.
    identity: Option<crate::run_identity::RunIdentity>,
}

impl ObserverJob {
    /// A notification can wait or go; a permission event is one a person is
    /// waiting on.
    fn is_notification(&self) -> bool {
        self.stage == ObserverStage::Notification.name()
    }
}

type Runner = Arc<dyn Fn(ObserverJob) + Send + Sync>;

/// Events lost to a full queue, counted by stage and session so each count
/// reaches the ledger of the session whose events they were.
struct Dropped {
    stage: &'static str,
    session: Option<String>,
    /// Where to record the count; `None` for a run with no ledger.
    events: Option<Arc<dyn HookEvents>>,
    count: u64,
}

struct QueueState {
    jobs: VecDeque<ObserverJob>,
    /// Drops not yet recorded.
    dropped: Vec<Dropped>,
    /// When the last `hook.dropped` batch was recorded.
    last_record: Option<Instant>,
    running: bool,
}

impl QueueState {
    fn note_dropped(&mut self, job: ObserverJob) {
        let session = job
            .identity
            .as_ref()
            .map(|identity| identity.session.clone());
        match self
            .dropped
            .iter_mut()
            .find(|entry| entry.stage == job.stage && entry.session == session)
        {
            Some(entry) => entry.count += 1,
            None => self.dropped.push(Dropped {
                stage: job.stage,
                session,
                events: job.events,
                count: 1,
            }),
        }
    }

    /// The tallies to record now: all of them, when the interval since the
    /// last record is up (or `force`).
    fn take_due(&mut self, interval: Duration, force: bool) -> Vec<Dropped> {
        if self.dropped.is_empty() {
            return Vec::new();
        }
        if !force && self.last_record.is_some_and(|at| at.elapsed() < interval) {
            return Vec::new();
        }
        self.last_record = Some(Instant::now());
        std::mem::take(&mut self.dropped)
    }

    /// How long until a waiting tally is due, if one is waiting.
    fn until_due(&self, interval: Duration) -> Option<Duration> {
        if self.dropped.is_empty() {
            return None;
        }
        Some(
            self.last_record
                .map_or(Duration::ZERO, |at| interval.saturating_sub(at.elapsed())),
        )
    }

    /// The next event to run: a permission event ahead of a waiting
    /// notification, otherwise the oldest.
    fn next_index(&self) -> Option<usize> {
        if self.jobs.is_empty() {
            return None;
        }
        Some(
            self.jobs
                .iter()
                .position(|job| !job.is_notification())
                .unwrap_or(0),
        )
    }
}

/// A line on stderr that cannot panic: a closed stderr must not cost a
/// session (or the worker) anything.
fn say(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// [`report_dropped`] to the real destinations, whatever a recorder does: a
/// recorder that panics costs that report, never the caller or the worker.
fn report_guarded(due: Vec<Dropped>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report_dropped(due, &mut |line| say(&line));
    }));
}

/// Say what a batch of drops cost: to the ledger of the session the events
/// were from, or on stderr for a run with none.
fn report_dropped(due: Vec<Dropped>, warn: &mut dyn FnMut(String)) {
    for entry in due {
        match entry.events {
            Some(events) => events.dropped(entry.stage, entry.count),
            None => warn(format!(
                "warning: {} {} observer event(s) were dropped: the hooks could not keep up, \
and this run has no ledger to record it in",
                entry.count, entry.stage
            )),
        }
    }
}

/// A bounded queue with one worker thread, started on first use.
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
                dropped: Vec::new(),
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

    /// Queue `job`. When the queue is full the oldest notification goes (and
    /// the new one itself, if the queue holds nothing but permission events):
    /// a permission event is evicted only by another. Never waits for a hook.
    pub fn submit(self: &Arc<Self>, job: ObserverJob) {
        self.ensure_worker();
        let due;
        {
            let mut state = self.lock();
            let mut job = Some(job);
            if state.jobs.len() >= self.capacity {
                let incoming_is_notification =
                    job.as_ref().is_some_and(ObserverJob::is_notification);
                let victim = state.jobs.iter().position(ObserverJob::is_notification);
                match victim {
                    Some(at) => {
                        if let Some(dropped) = state.jobs.remove(at) {
                            state.note_dropped(dropped);
                        }
                    }
                    None if incoming_is_notification => {
                        if let Some(incoming) = job.take() {
                            state.note_dropped(incoming);
                        }
                    }
                    None => {
                        if let Some(dropped) = state.jobs.pop_front() {
                            state.note_dropped(dropped);
                        }
                    }
                }
            }
            if let Some(job) = job {
                state.jobs.push_back(job);
            }
            due = state.take_due(self.interval, false);
        }
        self.ready.notify_one();
        report_guarded(due);
    }

    fn ensure_worker(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("rapid-observer-hooks".to_owned())
            .spawn(move || queue.work());
        if spawned.is_err() {
            // The next event tries again.
            self.started.store(false, Ordering::Release);
        }
    }

    fn work(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock();
                loop {
                    if let Some(at) = state.next_index()
                        && let Some(job) = state.jobs.remove(at)
                    {
                        state.running = true;
                        break job;
                    }
                    state.running = false;
                    self.idle.notify_all();
                    // A tally that is waiting out its interval is recorded
                    // when the interval is up, with nothing else to do.
                    match state.until_due(self.interval) {
                        None => {
                            state = self
                                .ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        Some(left) => {
                            let (next, timeout) = self
                                .ready
                                .wait_timeout(state, left)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state = next;
                            if timeout.timed_out() {
                                let due = state.take_due(self.interval, false);
                                if !due.is_empty() {
                                    // Still at work while the record is
                                    // written: a drain waits for it.
                                    state.running = true;
                                    drop(state);
                                    report_guarded(due);
                                    state = self.lock();
                                    state.running = false;
                                    self.idle.notify_all();
                                }
                            }
                        }
                    }
                }
            };
            // A runner that panics costs that event, not the worker.
            let runner = Arc::clone(&self.runner);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || runner(job)));
            let due = self.lock().take_due(self.interval, false);
            report_guarded(due);
        }
    }

    /// Wait until every event queued so far has run (tests, and process
    /// exit), then record whatever drops are still unrecorded. `false` when
    /// the limit passed first.
    pub fn drain(&self, limit: Duration) -> bool {
        self.drain_with(limit, &mut |line| say(&line))
    }

    fn drain_with(&self, limit: Duration, warn: &mut dyn FnMut(String)) -> bool {
        let deadline = Instant::now() + limit;
        let mut state = self.lock();
        let mut finished = true;
        while !state.jobs.is_empty() || state.running {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                finished = false;
                break;
            };
            let (next, timeout) = self
                .idle
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() && (!state.jobs.is_empty() || state.running) {
                finished = false;
                break;
            }
        }
        let due = state.take_due(self.interval, true);
        drop(state);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            report_dropped(due, warn);
        }));
        finished
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

/// Run one observer event: its hooks, in order, with the run's session and
/// turn in their environment; a hook that fails is a recorded `hook.failed`
/// (or a line on stderr for a run with no ledger). Whatever the hooks print or
/// decide is ignored.
fn run_job(job: ObserverJob) {
    let report = crate::run_identity::scoped(job.identity.as_ref(), || {
        crate::hooks::run_observer_hooks(
            job.stage,
            &job.entries,
            job.subject.as_deref(),
            &job.input,
            HOOK_TIMEOUT,
        )
    });
    match &job.events {
        Some(events) => {
            for failure in &report.failures {
                events.failed("", "", failure);
            }
        }
        None => {
            for failure in &report.failures {
                say(&format!(
                    "warning: {} hook {} failed: {}",
                    failure.event, failure.hook, failure.detail
                ));
            }
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
    // the recorder has none. The worker is neither, so the hooks' environment
    // is built from this identity.
    let thread = crate::run_identity::current();
    let session = events
        .as_ref()
        .and_then(|events| events.session())
        .or_else(|| thread.as_ref().map(|identity| identity.session.clone()));
    let turn = thread.and_then(|identity| identity.turn);
    if let Some(session) = &session {
        payload
            .entry("session_id")
            .or_insert_with(|| session.clone().into());
    }
    if let Some(turn) = &turn {
        payload
            .entry("turn_id")
            .or_insert_with(|| turn.clone().into());
    }
    queue.submit(ObserverJob {
        stage: stage.name(),
        entries: entries.to_vec(),
        subject: subject.map(str::to_owned),
        input: serde_json::Value::Object(payload).to_string(),
        events,
        identity: session.map(|session| crate::run_identity::RunIdentity { session, turn }),
    });
}

/// The secrets a scrubber has been told about (the credentials of the models
/// its sessions use), kept to scrub free text before a hook is handed it. Only
/// ever added to: scrubbing one secret too many is harmless.
#[derive(Default)]
struct Scrubber {
    secrets: Vec<String>,
    snapshot: Option<security::RedactionSnapshot>,
}

/// Shorter than this is a placeholder, not a credential: scrubbing it would
/// mangle ordinary text.
const MIN_SECRET_BYTES: usize = 8;
/// How many distinct secrets a process scrubs for.
const MAX_SECRETS: usize = 64;

/// What registering a secret came to.
#[derive(Debug, Eq, PartialEq)]
enum Registered {
    Added,
    /// Too short to be a credential, or already known.
    Skipped,
    /// Too many, or the registry refused it: the text may carry it.
    Failed,
}

impl Scrubber {
    fn register(&mut self, secret: &str) -> Registered {
        if secret.len() < MIN_SECRET_BYTES || self.secrets.iter().any(|known| known == secret) {
            return Registered::Skipped;
        }
        let mut candidates = self.secrets.clone();
        candidates.push(secret.to_owned());
        if candidates.len() > MAX_SECRETS {
            return Registered::Failed;
        }
        let cancel = security::RedactionCancellation::new();
        let mut registry = security::SecretRedactionRegistry::new();
        for (index, known) in candidates.iter().enumerate() {
            let registered = auth::SecretRef::from_alias(&format!("observer-secret-{index}"))
                .ok()
                .is_some_and(|refer| {
                    registry
                        .register_canary(&refer, known.as_bytes(), &cancel)
                        .is_ok()
                });
            if !registered {
                return Registered::Failed;
            }
        }
        self.secrets = candidates;
        self.snapshot = (registry.registered_count() > 0).then(|| registry.snapshot());
        Registered::Added
    }
}

fn scrubber() -> &'static Mutex<Scrubber> {
    static SCRUBBER: OnceLock<Mutex<Scrubber>> = OnceLock::new();
    SCRUBBER.get_or_init(|| Mutex::new(Scrubber::default()))
}

/// Tell the observer stages a secret value to scrub from the text they pass
/// to hooks. One that cannot be registered is said once on stderr; it is never
/// silently left unscrubbed.
pub(crate) fn register_secret(secret: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let outcome = scrubber()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .register(secret);
    if outcome == Registered::Failed && !WARNED.swap(true, Ordering::AcqRel) {
        say(
            "warning: a secret could not be registered to scrub the text observer hooks \
are handed; that text may carry it",
        );
    }
}

/// `text` with every registered secret replaced.
fn scrub(text: &str) -> String {
    // The scan runs outside the lock.
    let snapshot = scrubber()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .snapshot
        .clone();
    let Some(snapshot) = snapshot else {
        return text.to_owned();
    };
    let cancel = security::RedactionCancellation::new();
    match snapshot.redact_text(security::TextSink::Event, text, &cancel) {
        Ok(redacted) => redacted
            .as_text()
            .map(str::to_owned)
            .unwrap_or_else(|_| "(redacted)".to_owned()),
        // A scrub that could not finish says nothing rather than risk a leak.
        Err(_) => "(redacted)".to_owned(),
    }
}

/// `text` scrubbed of known secrets, then cut to `max` bytes on a character
/// boundary with control characters flattened to spaces (a hook reads this as
/// a line of JSON text, not as terminal output). Scrubbed before it is cut, so
/// a secret straddling the bound leaves no prefix behind.
pub fn bounded(text: &str, max: usize) -> String {
    let flattened: String = scrub(text)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let mut end = flattened.len().min(max);
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
        Some(source) if source.starts_with("mcp:") => bounded(source, MAX_TITLE_BYTES),
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
    if !is_configured(hooks, ObserverStage::PermissionRequested) {
        return;
    }
    emit(
        hooks,
        events,
        ObserverStage::PermissionRequested,
        Some(request.tool),
        serde_json::json!({
            "approval_id": request.approval_id,
            "tool": request.tool,
            "summary": bounded(request.summary, MAX_SUMMARY_BYTES),
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
    if !is_configured(hooks, ObserverStage::PermissionResolved) {
        return;
    }
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

/// How long a project's hooks, once read, are used before the settings are
/// read again: an event source fires on every job end and loop result, and
/// reading the settings (and the managed policy) for each would be most of
/// the cost of the event.
const HOOKS_CACHE_TTL: Duration = Duration::from_secs(2);

/// The hooks of the trusted project at `root`, read again when the last read
/// is older than [`HOOKS_CACHE_TTL`], so a hook added mid-session is
/// honoured within moments.
pub(crate) fn project_hooks(root: &std::path::Path) -> HooksConfig {
    type Cache = std::collections::HashMap<std::path::PathBuf, (Instant, HooksConfig)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Cache::new()));
    {
        let cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, hooks)) = cache.get(root)
            && at.elapsed() < HOOKS_CACHE_TTL
        {
            return hooks.clone();
        }
    }
    // Read outside the lock: it parses files.
    let hooks = crate::interactive::load_project_integrations(root).hooks;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), (Instant::now(), hooks.clone()));
    hooks
}

/// Where an asynchronous event source (a job table, a child's lifecycle, a
/// loop) finds the hooks to run when something it watches ends: the project's
/// settings are read when the event happens (at most every two seconds), not
/// when the source was built, so a hook added mid-session is honoured. Hooks
/// are project settings, so nothing is read for an untrusted project.
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
        let hooks = project_hooks(&self.root);
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
    if !is_configured(hooks, ObserverStage::Notification) {
        return;
    }
    emit(
        hooks,
        events,
        ObserverStage::Notification,
        Some(notice.kind),
        serde_json::json!({
            "kind": notice.kind,
            "title": bounded(notice.title, MAX_TITLE_BYTES),
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
        session: Option<String>,
    }

    impl Recording {
        fn of_session(session: &str) -> Arc<Self> {
            Arc::new(Self {
                session: Some(session.to_owned()),
                ..Self::default()
            })
        }

        fn total_dropped(&self) -> u64 {
            self.dropped
                .lock()
                .expect("dropped")
                .iter()
                .map(|(_, c)| c)
                .sum()
        }
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
        fn session(&self) -> Option<String> {
            self.session.clone()
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
    fn drops_are_recorded_at_most_once_per_interval_and_the_rest_at_exit() {
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
        // Eighteen events were lost; an hour-long interval allows the first
        // drop's record and no other while the run goes on.
        assert_eq!(
            *events.dropped.lock().expect("dropped"),
            [("notification".to_owned(), 1)],
            "one record so far, carrying the first drop"
        );
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        // The exit drain records the rest: nothing is lost to the rate limit.
        assert_eq!(
            *events.dropped.lock().expect("dropped"),
            [
                ("notification".to_owned(), 1),
                ("notification".to_owned(), 17)
            ]
        );
        assert_eq!(events.total_dropped(), 18);
    }

    #[test]
    fn a_burst_that_goes_quiet_is_recorded_when_its_interval_is_up() {
        let (queue, release, _seen) = held_queue(1, Duration::from_millis(150));
        let events = Arc::new(Recording::default());
        let shared: Arc<dyn HookEvents> = events.clone();
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, Some(shared.clone()), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for n in 2..=9 {
            notify(&queue, &hooks, Some(shared.clone()), n);
        }
        release.send(()).expect("release");
        // Nothing more is emitted and nobody drains: the worker, idle, still
        // reports what it dropped once the interval is up.
        let deadline = Instant::now() + Duration::from_secs(10);
        while events.total_dropped() < 7 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            events.total_dropped(),
            7,
            "{:?}",
            events.dropped.lock().expect("dropped")
        );
    }

    #[test]
    fn drops_are_recorded_in_the_ledger_of_the_session_they_were_from() {
        // An hour-long interval: after the first record, drops are tallied
        // and reported together, and each tally belongs to one session.
        let (queue, release, _seen) = held_queue(1, Duration::from_secs(3600));
        let alpha = Recording::of_session("alpha");
        let beta = Recording::of_session("beta");
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        let as_events = |recording: &Arc<Recording>| -> Option<Arc<dyn HookEvents>> {
            Some(recording.clone() as Arc<dyn HookEvents>)
        };
        notify(&queue, &hooks, as_events(&alpha), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Each arrival pushes out the one before it (capacity one): alpha 2
        // is lost to beta 3, beta 3 to beta 4, beta 4 to alpha 5, alpha 5 to
        // beta 6.
        notify(&queue, &hooks, as_events(&alpha), 2);
        notify(&queue, &hooks, as_events(&beta), 3);
        notify(&queue, &hooks, as_events(&beta), 4);
        notify(&queue, &hooks, as_events(&alpha), 5);
        notify(&queue, &hooks, as_events(&beta), 6);
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(alpha.total_dropped(), 2, "events 2 and 5 were alpha's");
        assert_eq!(beta.total_dropped(), 2, "events 3 and 4 were beta's");
    }

    /// A capacity-two queue with event 1 running (held), for the drop-policy
    /// tests.
    struct HeldPermissionQueue {
        queue: Arc<ObserverQueue>,
        release: mpsc::Sender<()>,
        seen: Arc<Mutex<Vec<u64>>>,
        events: Arc<Recording>,
        hooks: HooksConfig,
    }

    fn permission_queue() -> HeldPermissionQueue {
        let (queue, release, seen) = held_queue(2, Duration::ZERO);
        let events = Arc::new(Recording::default());
        let mut hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        hooks.permission_requested.push(HookEntry::plain("true"));
        let shared: Arc<dyn HookEvents> = events.clone();
        notify(&queue, &hooks, Some(shared), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        HeldPermissionQueue {
            queue,
            release,
            seen,
            events,
            hooks,
        }
    }

    fn permission(
        queue: &Arc<ObserverQueue>,
        hooks: &HooksConfig,
        events: &Arc<Recording>,
        n: u64,
    ) {
        emit_on(
            queue,
            hooks,
            Some(events.clone() as Arc<dyn HookEvents>),
            ObserverStage::PermissionRequested,
            Some("shell_exec"),
            serde_json::json!({ "n": n }),
        );
    }

    #[test]
    fn a_full_queue_gives_up_a_notification_before_a_permission_event() {
        let HeldPermissionQueue {
            queue,
            release,
            seen,
            events,
            hooks,
        } = permission_queue();
        let shared: Arc<dyn HookEvents> = events.clone();
        // Event 1 is running. A notification and a permission event wait.
        notify(&queue, &hooks, Some(shared.clone()), 2);
        permission(&queue, &hooks, &events, 3);
        // Full: another permission event pushes out the notification (2), not
        // the permission event (3) that is older than it.
        permission(&queue, &hooks, &events, 4);
        // Full of permission events: a notification does not push one out. It
        // is the one that goes.
        notify(&queue, &hooks, Some(shared), 5);
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [1, 3, 4]);
        assert_eq!(events.total_dropped(), 2, "notifications 2 and 5");
    }

    #[test]
    fn a_permission_event_is_dropped_only_to_make_room_for_another() {
        let HeldPermissionQueue {
            queue,
            release,
            seen,
            events,
            hooks,
        } = permission_queue();
        permission(&queue, &hooks, &events, 2);
        permission(&queue, &hooks, &events, 3);
        permission(&queue, &hooks, &events, 4);
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [1, 3, 4], "the oldest went");
        assert_eq!(events.total_dropped(), 1);
    }

    #[test]
    fn a_waiting_permission_event_runs_before_a_waiting_notification() {
        let (queue, release, seen) = held_queue(8, Duration::from_secs(60));
        let hooks = {
            let mut hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
            hooks.permission_resolved.push(HookEntry::plain("true"));
            hooks
        };
        notify(&queue, &hooks, None, 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        notify(&queue, &hooks, None, 2);
        notify(&queue, &hooks, None, 3);
        emit_on(
            &queue,
            &hooks,
            None,
            ObserverStage::PermissionResolved,
            Some("shell_exec"),
            serde_json::json!({ "n": 4 }),
        );
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [1, 4, 2, 3]);
    }

    #[test]
    fn a_runner_that_panics_costs_that_event_not_the_worker() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let runner: Runner = Arc::new(move |job: ObserverJob| {
            let n = serde_json::from_str::<serde_json::Value>(&job.input)
                .ok()
                .and_then(|value| value["n"].as_u64())
                .unwrap_or(u64::MAX);
            assert_ne!(n, 1, "the first event's runner panics");
            record.lock().expect("seen").push(n);
        });
        let queue = ObserverQueue::new(8, Duration::from_secs(60), runner);
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, None, 1);
        notify(&queue, &hooks, None, 2);
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [2]);
    }

    #[test]
    fn drops_in_a_run_with_no_ledger_are_said_not_lost() {
        let (queue, release, _seen) = held_queue(1, Duration::from_secs(3600));
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, None, 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for n in 2..=5 {
            notify(&queue, &hooks, None, n);
        }
        release.send(()).expect("release");
        let mut warnings = Vec::new();
        assert!(queue.drain_with(Duration::from_secs(5), &mut |line| warnings.push(line)));
        // The first drop was said as it happened (on stderr); the rest, here.
        assert!(
            warnings.iter().any(
                |w| w.contains("notification observer event(s) were dropped")
                    && w.contains("no ledger")
            ),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_hook_is_given_the_session_and_turn_in_its_environment() {
        let dir = std::env::temp_dir().join(format!("observer-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("env.txt");
        let path = dir.join("hook.sh");
        std::fs::write(
            &path,
            format!(
                "printf '%s|%s' \"$RAPIDLM_SESSION_ID\" \"$RAPIDLM_TURN_ID\" > {}\ncat >/dev/null\n",
                test_fixtures::sh_quote(&capture)
            ),
        )
        .expect("script");
        let command = format!("sh {}", test_fixtures::slash_path(&path));
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain(command));
        let notice = Notice {
            kind: "turn_complete",
            title: "done",
            outcome: Some("ok"),
            job_id: None,
            loop_id: None,
            agent_id: None,
        };
        let identity = crate::run_identity::RunIdentity {
            session: "sess-7".to_owned(),
            turn: Some("turn-3".to_owned()),
        };
        // The emitting thread has the identity; the worker, a thread of its
        // own, must still hand it to the hook.
        crate::run_identity::scoped(Some(&identity), || notification(&hooks, None, &notice));
        assert!(drain(Duration::from_secs(10)));
        assert_eq!(
            std::fs::read_to_string(&capture).expect("env captured"),
            "sess-7|turn-3"
        );
        // A recorder that knows its session supplies it when the thread has none.
        let events: Arc<dyn HookEvents> = Recording::of_session("sess-9");
        notification(&hooks, Some(events), &notice);
        assert!(drain(Duration::from_secs(10)));
        assert_eq!(
            std::fs::read_to_string(&capture).expect("env captured"),
            "sess-9|",
            "the session, and no turn the thread did not have"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // Had either been queued, its runner would be parked on the gate and
        // the drain would time out.
        assert!(
            queue.drain(Duration::from_millis(500)),
            "nothing was queued, so there is nothing to wait for"
        );
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
    fn project_hooks_are_read_again_only_after_the_cache_interval() {
        let dir = std::env::temp_dir().join(format!("observer-hooks-cache-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".rapidlm")).expect("dir");
        let settings = dir.join(".rapidlm").join("settings.json");
        std::fs::write(&settings, r#"{"hooks":{"notification":["true"]}}"#).expect("settings");
        assert!(is_configured(
            &project_hooks(&dir),
            ObserverStage::Notification
        ));
        std::fs::write(&settings, "{}").expect("settings");
        assert!(
            is_configured(&project_hooks(&dir), ObserverStage::Notification),
            "within the interval the earlier read is used"
        );
        std::thread::sleep(HOOKS_CACHE_TTL + Duration::from_millis(300));
        assert!(
            !is_configured(&project_hooks(&dir), ObserverStage::Notification),
            "after it the settings are read again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drain_waits_for_a_tally_that_is_being_recorded() {
        // A record made by the worker takes a while and says when it has
        // started and when it is done; one made by a submitter is instant.
        struct SlowSink {
            started: AtomicBool,
            done: AtomicBool,
        }
        impl HookEvents for SlowSink {
            fn decided(&self, _: &str, _: &str, _: &crate::hooks::HookDecisionRecord) {}
            fn rewritten(&self, _: &str, _: &str, _: &crate::exec_tools::HookRewriteRecord) {}
            fn failed(&self, _: &str, _: &str, _: &crate::hooks::HookFailure) {}
            fn dropped(&self, _: &str, _: u64) {
                if std::thread::current().name() == Some("rapid-observer-hooks") {
                    self.started.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(500));
                    self.done.store(true, Ordering::SeqCst);
                }
            }
        }
        let (queue, release, _seen) = held_queue(1, Duration::from_millis(300));
        let sink = Arc::new(SlowSink {
            started: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let events: Option<Arc<dyn HookEvents>> = Some(sink.clone());
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, events.clone(), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The first drop is recorded at once, by this thread; the rest are
        // tallied and left for the idle worker when the interval is up.
        for n in 2..=6 {
            notify(&queue, &hooks, events.clone(), n);
        }
        release.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sink.started.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(sink.started.load(Ordering::SeqCst), "the idle flush began");
        // The worker is writing the record now: a drain does not return before
        // it is written.
        assert!(queue.drain(Duration::from_secs(5)));
        assert!(
            sink.done.load(Ordering::SeqCst),
            "drain returned while a tally was still being recorded"
        );
    }

    #[test]
    fn a_recorder_that_panics_costs_that_report_not_the_worker() {
        struct Panicking;
        impl HookEvents for Panicking {
            fn decided(&self, _: &str, _: &str, _: &crate::hooks::HookDecisionRecord) {}
            fn rewritten(&self, _: &str, _: &str, _: &crate::exec_tools::HookRewriteRecord) {}
            fn failed(&self, _: &str, _: &str, _: &crate::hooks::HookFailure) {}
            fn dropped(&self, _: &str, _: u64) {
                panic!("the recorder fails");
            }
        }
        let (queue, release, seen) = held_queue(1, Duration::ZERO);
        let events: Option<Arc<dyn HookEvents>> = Some(Arc::new(Panicking));
        let hooks = config_with(ObserverStage::Notification, HookEntry::plain("true"));
        notify(&queue, &hooks, events.clone(), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.queued() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Two more: the second pushes the first out, and the recorder panics
        // on the submitter's thread, which carries on.
        notify(&queue, &hooks, events.clone(), 2);
        notify(&queue, &hooks, events.clone(), 3);
        release.send(()).expect("release");
        assert!(queue.drain(Duration::from_secs(5)));
        // The worker survived to run what was left, and to run more.
        notify(&queue, &hooks, None, 4);
        assert!(queue.drain(Duration::from_secs(5)));
        assert_eq!(*seen.lock().expect("seen"), [1, 3, 4]);
    }

    #[test]
    fn a_scrubber_ignores_placeholders_caps_its_secrets_and_says_when_it_cannot() {
        let mut scrubber = Scrubber::default();
        assert_eq!(scrubber.register("short"), Registered::Skipped);
        assert_eq!(
            scrubber.register("a-real-looking-secret-0001"),
            Registered::Added
        );
        assert_eq!(
            scrubber.register("a-real-looking-secret-0001"),
            Registered::Skipped
        );
        for n in 1..MAX_SECRETS {
            assert_eq!(
                scrubber.register(&format!("another-secret-value-{n:04}")),
                Registered::Added,
                "{n}"
            );
        }
        assert_eq!(scrubber.secrets.len(), MAX_SECRETS);
        assert_eq!(
            scrubber.register("one-secret-too-many-0000"),
            Registered::Failed
        );
        assert_eq!(
            scrubber.secrets.len(),
            MAX_SECRETS,
            "the failure changed nothing"
        );
    }

    #[test]
    fn the_documented_bounds_hold_at_the_call_sites() {
        let dir = std::env::temp_dir().join(format!("observer-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("events.jsonl");
        let path = dir.join("hook.sh");
        std::fs::write(
            &path,
            format!(
                "cat >> {file}\nprintf '\\n' >> {file}\n",
                file = test_fixtures::sh_quote(&capture)
            ),
        )
        .expect("script");
        let command = format!("sh {}", test_fixtures::slash_path(&path));
        let mut hooks = config_with(
            ObserverStage::PermissionRequested,
            HookEntry::plain(command.clone()),
        );
        hooks.notification.push(HookEntry::plain(command));
        let long = "t".repeat(700);
        permission_requested(
            &hooks,
            None,
            &RequestedApproval {
                approval_id: "tok",
                tool: "shell_exec",
                summary: &long,
                source: None,
                arguments_digest: None,
                default_source: "lattice",
            },
        );
        notification(
            &hooks,
            None,
            &Notice {
                kind: "turn_complete",
                title: &long,
                outcome: Some("ok"),
                job_id: None,
                loop_id: None,
                agent_id: None,
            },
        );
        assert!(drain(Duration::from_secs(10)));
        let events: Vec<serde_json::Value> = std::fs::read_to_string(&capture)
            .expect("captured")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        let summary = events
            .iter()
            .find(|e| e["event"] == "permission_requested")
            .expect("requested");
        let title = events
            .iter()
            .find(|e| e["event"] == "notification")
            .expect("notification");
        assert_eq!(
            summary["summary"].as_str().map(str::len),
            Some(MAX_SUMMARY_BYTES)
        );
        assert_eq!(title["title"].as_str().map(str::len), Some(MAX_TITLE_BYTES));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_registered_secret_is_scrubbed_before_the_text_is_cut() {
        register_secret("sk-observer-test-secret-0123456789");
        let secret = "sk-observer-test-secret-0123456789";
        let straddling = format!("{}{secret} tail", "x".repeat(MAX_TITLE_BYTES - 10));
        let cut = bounded(&straddling, MAX_TITLE_BYTES);
        assert!(
            !cut.contains("sk-observer"),
            "no prefix of the secret is left: {cut}"
        );
        assert!(cut.len() <= MAX_TITLE_BYTES);
        let whole = bounded(&format!("run with {secret} now"), MAX_SUMMARY_BYTES);
        assert!(
            !whole.contains(secret) && whole.starts_with("run with "),
            "{whole}"
        );
        // The documented bounds: 512 for a summary, 200 for a title.
        let long = "s".repeat(600);
        assert_eq!(bounded(&long, MAX_SUMMARY_BYTES).len(), 512);
        assert_eq!(bounded(&long, MAX_TITLE_BYTES).len(), 200);
    }

    #[test]
    fn free_text_is_cut_on_a_character_boundary() {
        let text = "é".repeat(400);
        let cut = bounded(&text, MAX_TITLE_BYTES);
        assert!(cut.len() <= MAX_TITLE_BYTES && cut.chars().all(|c| c == 'é'));
        assert!(bounded(&text, MAX_SUMMARY_BYTES).len() > MAX_TITLE_BYTES);
        assert_eq!(bounded("a\tb", MAX_TITLE_BYTES), "a b");
        assert!(NOTIFICATION_KINDS.contains(&"goal_changed"));
        assert_eq!(entries(["x"]).len(), 1);
    }
}
