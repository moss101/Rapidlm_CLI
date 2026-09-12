//! Replay-then-tail subscription over committed ledger events.
//!
//! `subscribe(session, after_seq)` first replays the durable gap
//! `(after_seq, last_seq]` then tails newly committed rows. Live delivery
//! uses a bounded channel: a slow consumer is disconnected with a resume
//! cursor instead of blocking [`crate::ledger::EventLedger::append`].

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use protocol::SessionId;

use crate::event::ErasedEventEnvelope;
use crate::ledger::{CancellationToken, EventLedger, LedgerError};

/// In-memory live-tail capacity. Replay still drains from SQLite one event at
/// a time; this bound only limits events waiting for a consumer.
pub const DEFAULT_LIVE_BOUND: usize = 64;

/// How long a live `send` waits before the subscriber is treated as lagged.
const LIVE_SEND_TIMEOUT: Duration = Duration::from_millis(100);

/// Poll interval while waiting for the next committed seq or a drain slot.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Stream of committed [`ErasedEventEnvelope`] values from a resume cursor.
pub struct EventStream {
    rx: Option<Receiver<ErasedEventEnvelope>>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    stop: CancellationToken,
    /// Last seq delivered to the caller (`0` if none). Reconnect with this.
    cursor: u64,
    worker: Option<JoinHandle<()>>,
}

/// Shared worker/consumer flags. `error` is taken at most once.
struct Shared {
    lagged: AtomicBool,
    error: Mutex<Option<LedgerError>>,
}

/// Terminal and setup failures for an [`EventStream`].
#[derive(Debug)]
pub enum SubscriptionError {
    Cancelled { resume_cursor: u64 },
    Lagged { resume_cursor: u64 },
    Ledger(LedgerError),
}

impl EventLedger {
    /// Subscribe after `after_seq`. The first delivered event is `after_seq + 1`.
    ///
    /// Replays already-committed events through the subscribe-time `last_seq`,
    /// then tails live commits. The live channel is bounded
    /// ([`DEFAULT_LIVE_BOUND`]); overflow disconnects with
    /// [`SubscriptionError::Lagged`] and a reconnect cursor. Append never waits
    /// on this path.
    pub fn subscribe(
        &self,
        session: SessionId,
        after_seq: u64,
        cancel: &CancellationToken,
    ) -> Result<EventStream, LedgerError> {
        self.subscribe_bounded(session, after_seq, DEFAULT_LIVE_BOUND, cancel)
    }

    /// [`Self::subscribe`] with an explicit live-channel bound (at least 1).
    pub fn subscribe_bounded(
        &self,
        session: SessionId,
        after_seq: u64,
        live_bound: usize,
        cancel: &CancellationToken,
    ) -> Result<EventStream, LedgerError> {
        cancel.check()?;
        let live_bound = live_bound.max(1);
        let replay_through = self.last_seq(session, cancel)?;
        if after_seq > replay_through {
            return Err(LedgerError::EventNotFound {
                session_id: session,
                seq: after_seq,
            });
        }

        let (tx, rx) = sync_channel(live_bound);
        let shared = Arc::new(Shared::new());
        let stop = CancellationToken::new();
        let worker_ledger = self.clone();
        let worker_cancel = cancel.clone();
        let worker_stop = stop.clone();
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name(format!("ledger-sub-{session}"))
            .spawn(move || {
                let mut worker = Worker {
                    ledger: worker_ledger,
                    session,
                    cancel: worker_cancel,
                    stop: worker_stop,
                    tx,
                    shared: worker_shared,
                    cursor: after_seq,
                };
                if worker.replay_gap(replay_through) {
                    worker.tail_live();
                }
            })
            .map_err(LedgerError::Io)?;

        Ok(EventStream {
            rx: Some(rx),
            shared,
            cancel: cancel.clone(),
            stop,
            cursor: after_seq,
            worker: Some(handle),
        })
    }
}

impl EventStream {
    /// Last seq delivered to this consumer (`after_seq` if nothing received).
    ///
    /// Reconnect with `subscribe(session, cursor)` to receive `cursor + 1`
    /// exactly once at the API level.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Wait for the next committed event after [`Self::cursor`].
    pub fn recv(&mut self) -> Result<ErasedEventEnvelope, SubscriptionError> {
        loop {
            if let Some(error) = self.terminal_if_stopped() {
                return Err(error);
            }
            let rx = self.rx.as_ref().ok_or_else(|| self.closed_error())?;
            match rx.recv_timeout(TAIL_POLL_INTERVAL) {
                Ok(event) => {
                    self.cursor = event.seq();
                    return Ok(event);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(self.closed_error()),
            }
        }
    }

    /// Non-blocking poll. `Ok(None)` means no new committed event is queued.
    pub fn try_recv(&mut self) -> Result<Option<ErasedEventEnvelope>, SubscriptionError> {
        if let Some(error) = self.terminal_if_stopped() {
            return Err(error);
        }
        let rx = self.rx.as_ref().ok_or_else(|| self.closed_error())?;
        match rx.try_recv() {
            Ok(event) => {
                self.cursor = event.seq();
                Ok(Some(event))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(self.closed_error()),
        }
    }

    /// Stop the worker. Further `recv` returns [`SubscriptionError::Cancelled`].
    pub fn close(&mut self) {
        self.shutdown();
    }

    fn terminal_if_stopped(&self) -> Option<SubscriptionError> {
        if self.cancel.is_cancelled() || self.stop.is_cancelled() {
            Some(SubscriptionError::Cancelled {
                resume_cursor: self.cursor,
            })
        } else {
            None
        }
    }

    fn closed_error(&self) -> SubscriptionError {
        if self.cancel.is_cancelled() || self.stop.is_cancelled() {
            return SubscriptionError::Cancelled {
                resume_cursor: self.cursor,
            };
        }
        if self.shared.lagged.load(Ordering::SeqCst) {
            return SubscriptionError::Lagged {
                resume_cursor: self.cursor,
            };
        }
        if let Some(err) = self.shared.take_error() {
            return SubscriptionError::Ledger(err);
        }
        SubscriptionError::Cancelled {
            resume_cursor: self.cursor,
        }
    }

    fn shutdown(&mut self) {
        self.stop.cancel();
        self.rx.take();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Shared {
    fn new() -> Self {
        Self {
            lagged: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn take_error(&self) -> Option<LedgerError> {
        match self.error.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    fn set_error(&self, err: LedgerError) {
        let mut slot = match self.error.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if slot.is_none() {
            *slot = Some(err);
        }
    }
}

struct Worker {
    ledger: EventLedger,
    session: SessionId,
    cancel: CancellationToken,
    stop: CancellationToken,
    tx: SyncSender<ErasedEventEnvelope>,
    shared: Arc<Shared>,
    cursor: u64,
}

impl Worker {
    fn replay_gap(&mut self, replay_through: u64) -> bool {
        while self.cursor < replay_through {
            if self.stopped() {
                return false;
            }
            let Some(next) = self.next_seq() else {
                return false;
            };
            let Some(event) = self.load_event(next) else {
                return false;
            };
            if !enqueue_replay(&self.tx, event, &self.cancel, &self.stop) {
                return false;
            }
            self.cursor = next;
        }
        true
    }

    fn tail_live(&mut self) {
        loop {
            if self.stopped() {
                return;
            }
            let last = match self.ledger.last_seq(self.session, &self.cancel) {
                Ok(last) => last,
                Err(LedgerError::Cancelled) => return,
                Err(err) => {
                    self.shared.set_error(err);
                    return;
                }
            };
            if self.cursor >= last {
                thread::sleep(TAIL_POLL_INTERVAL);
                continue;
            }
            let Some(next) = self.next_seq() else {
                return;
            };
            let Some(event) = self.load_event(next) else {
                return;
            };
            match enqueue_live(&self.tx, event, &self.cancel, &self.stop) {
                SendResult::Sent => self.cursor = next,
                SendResult::Lagged => {
                    self.shared.lagged.store(true, Ordering::SeqCst);
                    return;
                }
                SendResult::Stopped => return,
            }
        }
    }

    fn load_event(&self, seq: u64) -> Option<ErasedEventEnvelope> {
        if self.stopped() {
            return None;
        }
        match self.ledger.get(self.session, seq, &self.cancel) {
            Ok(event) => Some(event),
            Err(LedgerError::Cancelled) => None,
            Err(err) => {
                self.shared.set_error(err);
                None
            }
        }
    }

    fn next_seq(&self) -> Option<u64> {
        next_seq(self.cursor, &self.shared)
    }

    fn stopped(&self) -> bool {
        stopped(&self.cancel, &self.stop)
    }
}

enum SendResult {
    Sent,
    Lagged,
    Stopped,
}

fn enqueue_replay(
    tx: &SyncSender<ErasedEventEnvelope>,
    event: ErasedEventEnvelope,
    cancel: &CancellationToken,
    stop: &CancellationToken,
) -> bool {
    matches!(enqueue(tx, event, None, cancel, stop), SendResult::Sent)
}

fn enqueue_live(
    tx: &SyncSender<ErasedEventEnvelope>,
    event: ErasedEventEnvelope,
    cancel: &CancellationToken,
    stop: &CancellationToken,
) -> SendResult {
    enqueue(tx, event, Some(LIVE_SEND_TIMEOUT), cancel, stop)
}

fn enqueue(
    tx: &SyncSender<ErasedEventEnvelope>,
    mut event: ErasedEventEnvelope,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
    stop: &CancellationToken,
) -> SendResult {
    let deadline = timeout.map(|limit| std::time::Instant::now() + limit);
    loop {
        if stopped(cancel, stop) {
            return SendResult::Stopped;
        }
        match tx.try_send(event) {
            Ok(()) => return SendResult::Sent,
            Err(TrySendError::Full(returned)) => {
                if deadline.is_some_and(|end| std::time::Instant::now() >= end) {
                    return SendResult::Lagged;
                }
                event = returned;
                thread::sleep(TAIL_POLL_INTERVAL);
            }
            Err(TrySendError::Disconnected(_)) => return SendResult::Stopped,
        }
    }
}

fn next_seq(cursor: u64, shared: &Shared) -> Option<u64> {
    match cursor.checked_add(1) {
        Some(next) => Some(next),
        None => {
            shared.set_error(LedgerError::SequenceExhausted);
            None
        }
    }
}

fn stopped(cancel: &CancellationToken, stop: &CancellationToken) -> bool {
    cancel.is_cancelled() || stop.is_cancelled()
}

impl fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { resume_cursor } => {
                write!(f, "event subscription cancelled at seq {resume_cursor}")
            }
            Self::Lagged { resume_cursor } => {
                write!(
                    f,
                    "event subscription lagged; reconnect from seq {resume_cursor}"
                )
            }
            Self::Ledger(err) => write!(f, "event subscription ledger error: {err}"),
        }
    }
}

impl std::error::Error for SubscriptionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(err) => Some(err),
            Self::Cancelled { .. } | Self::Lagged { .. } => None,
        }
    }
}

impl From<LedgerError> for SubscriptionError {
    fn from(value: LedgerError) -> Self {
        Self::Ledger(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ActorKind, ActorRef, EventKind};
    use crate::ledger::AppendOptions;
    use protocol::{EventId, ProjectId, RedactionClass, TraceId};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempLedger {
        path: PathBuf,
        ledger: EventLedger,
    }

    impl TempLedger {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-event-sub-{}-{seq}.sqlite",
                std::process::id()
            ));
            remove_db_files(&path);
            let ledger = EventLedger::open(&path).expect("open ledger");
            Self { path, ledger }
        }
    }

    impl Drop for TempLedger {
        fn drop(&mut self) {
            remove_db_files(&self.path);
        }
    }

    fn remove_db_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sidecar(path, "-shm"));
        let _ = std::fs::remove_file(sidecar(path, "-journal"));
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn options() -> AppendOptions {
        AppendOptions {
            redaction: RedactionClass::Project,
            trace_id: TraceId::new(),
            expected_seq: None,
        }
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::System, &EventId::new().to_string()).expect("actor")
    }

    fn seed_session(ledger: &EventLedger) -> SessionId {
        let session = SessionId::new();
        ledger
            .create_session(session, ProjectId::new(), &live())
            .expect("create session");
        session
    }

    fn append_n(ledger: &EventLedger, session: SessionId, n: usize) -> Vec<u64> {
        let mut seqs = Vec::with_capacity(n);
        for i in 0..n {
            let envelope = ledger
                .append(
                    session,
                    actor(),
                    EventKind::TurnStarted,
                    serde_json::json!({ "i": i }),
                    &options(),
                    &live(),
                )
                .expect("append");
            seqs.push(envelope.seq());
        }
        seqs
    }

    fn recv_n(stream: &mut EventStream, n: usize) -> Vec<u64> {
        let mut seqs = Vec::with_capacity(n);
        for _ in 0..n {
            let event = stream.recv().expect("recv");
            seqs.push(event.seq());
        }
        seqs
    }

    #[test]
    fn subscribe_replays_durable_gap_then_tails_live() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        assert_eq!(append_n(&tmp.ledger, session, 2), vec![1, 2]);

        let cancel = live();
        let mut stream = tmp
            .ledger
            .subscribe(session, 0, &cancel)
            .expect("subscribe");
        assert_eq!(recv_n(&mut stream, 2), vec![1, 2]);
        assert_eq!(stream.cursor(), 2);

        let live_event = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnCompleted,
                serde_json::json!({ "live": true }),
                &options(),
                &live(),
            )
            .expect("live append");
        assert_eq!(live_event.seq(), 3);
        let tailed = stream.recv().expect("tail");
        assert_eq!(tailed.seq(), 3);
        assert_eq!(tailed.kind(), EventKind::TurnCompleted);
        assert_eq!(stream.cursor(), 3);
    }

    #[test]
    fn reconnect_from_seq_n_receives_n_plus_one_exactly_once() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        append_n(&tmp.ledger, session, 5);

        let cancel = live();
        let mut first = tmp
            .ledger
            .subscribe(session, 0, &cancel)
            .expect("first subscribe");
        let first_batch = recv_n(&mut first, 3);
        assert_eq!(first_batch, vec![1, 2, 3]);
        let resume = first.cursor();
        assert_eq!(resume, 3);
        drop(first);

        let mut second = tmp
            .ledger
            .subscribe(session, resume, &cancel)
            .expect("reconnect");
        let second_batch = recv_n(&mut second, 2);
        assert_eq!(second_batch, vec![4, 5]);
        assert!(!second_batch.contains(&3));

        append_n(&tmp.ledger, session, 2);
        let live_batch = recv_n(&mut second, 2);
        assert_eq!(live_batch, vec![6, 7]);

        let mut all = first_batch;
        all.extend(second_batch);
        all.extend(live_batch);
        assert_eq!(all, vec![1, 2, 3, 4, 5, 6, 7]);
        let unique: std::collections::BTreeSet<u64> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "duplicate seq at API level: {all:?}"
        );
    }

    #[test]
    fn slow_subscriber_does_not_block_ledger_appends() {
        // The property: a subscriber that never reads, with a buffer of 4,
        // must not make the 32 appends behind it wait. An append path that
        // *did* wait on the subscriber would not be slow — it would never
        // return, with nothing draining a full buffer. So the appends run
        // on their own thread and the bound is a deadline generous enough
        // that only a blocked append can miss it; the earlier 750ms figure
        // was a disk-speed assertion in disguise (a shared Linux runner
        // took 785ms for 32 durable commits, a Windows one 2.7s).
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let cancel = live();
        let stream = tmp
            .ledger
            .subscribe_bounded(session, 0, 4, &cancel)
            .expect("subscribe empty");

        let ledger = tmp.ledger.clone();
        let appends = std::thread::spawn(move || append_n(&ledger, session, 32));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !appends.is_finished() {
            assert!(
                Instant::now() < deadline,
                "appends still not finished after 60s: a subscriber that never reads is blocking the ledger"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let seqs = appends.join().expect("append thread");
        assert_eq!(seqs, (1..=32).collect::<Vec<_>>());
        drop(stream);
    }

    #[test]
    fn lagged_live_subscriber_reconnects_without_duplicates_or_gaps() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let cancel = live();
        let mut stream = tmp
            .ledger
            .subscribe_bounded(session, 0, 2, &cancel)
            .expect("subscribe empty");

        append_n(&tmp.ledger, session, 16);
        // Allow the live send timeout to fire so lag is deterministic.
        thread::sleep(LIVE_SEND_TIMEOUT + TAIL_POLL_INTERVAL);

        let mut delivered = Vec::new();
        loop {
            match stream.recv() {
                Ok(event) => delivered.push(event.seq()),
                Err(SubscriptionError::Lagged { resume_cursor }) => {
                    assert_eq!(resume_cursor, stream.cursor());
                    assert_eq!(resume_cursor, *delivered.last().unwrap_or(&0));
                    break;
                }
                Err(other) => panic!("expected lag, got {other}"),
            }
        }
        assert!(!delivered.is_empty());
        let resume = stream.cursor();
        drop(stream);

        let mut resumed = tmp
            .ledger
            .subscribe(session, resume, &cancel)
            .expect("reconnect after lag");
        let remaining_count = 16 - delivered.len();
        let rest = recv_n(&mut resumed, remaining_count);
        assert_eq!(rest.first().copied(), Some(resume + 1));

        delivered.extend(rest);
        assert_eq!(delivered, (1..=16).collect::<Vec<_>>());
    }

    #[test]
    fn cancelled_subscription_returns_resume_cursor() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        append_n(&tmp.ledger, session, 2);
        let cancel = live();
        let mut stream = tmp
            .ledger
            .subscribe(session, 0, &cancel)
            .expect("subscribe");
        assert_eq!(stream.recv().expect("first").seq(), 1);
        cancel.cancel();
        match stream.recv() {
            Err(SubscriptionError::Cancelled { resume_cursor }) => {
                assert_eq!(resume_cursor, 1);
            }
            other => panic!("expected cancelled, got {other:?}"),
        }
    }

    #[test]
    fn missing_session_is_not_subscribed() {
        let tmp = TempLedger::create();
        let session = SessionId::new();
        match tmp.ledger.subscribe(session, 0, &live()) {
            Err(LedgerError::SessionNotFound { session_id }) if session_id == session => {}
            Err(err) => panic!("expected SessionNotFound, got {err}"),
            Ok(_) => panic!("subscribe succeeded for missing session"),
        }
    }

    #[test]
    fn cursor_ahead_of_ledger_is_rejected() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        append_n(&tmp.ledger, session, 1);
        match tmp.ledger.subscribe(session, 4, &live()) {
            Err(LedgerError::EventNotFound { session_id, seq: 4 }) if session_id == session => {}
            Err(err) => panic!("expected EventNotFound seq 4, got {err}"),
            Ok(_) => panic!("subscribe succeeded with cursor ahead of ledger"),
        }
    }
}
