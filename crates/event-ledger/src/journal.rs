//! Operation Journal: side-effect state distinct from the Event Ledger.
//!
//! External effects move `prepared → executing → committed | failed | uncertain
//! → reconciled`. Safe-replay effects may retry after crash; at-most-once
//! effects that crash while executing become uncertain and must be reconciled,
//! never re-executed. Hash-linked egress receipts record every attempt.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::{EventId, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ledger::EventLedger;

/// Maximum UTF-8 bytes in a fingerprint component (action/principal/target/preconditions).
pub const MAX_FINGERPRINT_FIELD_BYTES: usize = 4096;

/// Maximum UTF-8 bytes accepted in a wait token.
pub const MAX_WAIT_TOKEN_BYTES: usize = 128;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Cooperative cancellation for journal operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Durable operation identifier (UUIDv7).
pub type OperationId = EventId;

/// Closed operation-journal state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OperationState {
    Prepared,
    Executing,
    Committed,
    Failed,
    Uncertain,
    Reconciled,
}

/// Whether crash recovery may re-execute the effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IdempotencyClass {
    /// Effect is safe to replay (GET-like / content-addressed put).
    SafeReplay,
    /// Effect must not be re-executed; crash-in-flight requires reconcile.
    AtMostOnce,
}

/// Host recovery decision. Never returns Replay for at-most-once in-flight work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ReplayPolicy {
    Replay,
    Reconcile,
    None,
}

/// Canonical inputs to a stable effect fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectSpec {
    action: String,
    principal: String,
    target: String,
    preconditions: String,
}

/// SHA-256 of length-prefixed EffectSpec fields. Stable across process restarts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct EffectFingerprint([u8; 32]);

/// One journaled side effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRecord {
    id: OperationId,
    session_id: SessionId,
    fingerprint: EffectFingerprint,
    state: OperationState,
    idempotency: IdempotencyClass,
    reconcile_ref: Option<String>,
}

/// Hash-linked egress attempt. `prev_hash` is zeros for the first attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressReceipt {
    session_id: SessionId,
    operation_id: OperationId,
    attempt: u32,
    prev_hash: String,
    receipt_hash: String,
    outcome: EgressOutcome,
}

/// Attempt-level delivery result. Failures still produce a receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EgressOutcome {
    Attempted,
    Delivered,
    Failed,
}

/// Durable approval/wait record independent of in-memory graph wait tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitRecord {
    session_id: SessionId,
    wait_token: String,
    state: WaitState,
}

/// Pending or terminal wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WaitState {
    Pending,
    Approved,
    Denied,
    Expired,
}

/// File-backed Operation Journal over the same SQLite file as the Event Ledger.
#[derive(Clone, Debug)]
pub struct OperationJournal {
    ledger: EventLedger,
}

/// Typed journal failures. Invalid transitions fail closed.
#[derive(Debug)]
pub enum JournalError {
    Cancelled,
    SessionNotFound {
        session_id: SessionId,
    },
    NotFound {
        operation_id: OperationId,
    },
    WaitNotFound {
        wait_token: String,
    },
    InvalidTransition {
        from: OperationState,
        to: OperationState,
    },
    ReplayForbidden,
    InvalidField(&'static str),
    Conflict,
    Corrupt(&'static str),
    Sqlite(rusqlite::Error),
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), JournalError> {
        if self.is_cancelled() {
            Err(JournalError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectSpec {
    pub fn new(
        action: impl Into<String>,
        principal: impl Into<String>,
        target: impl Into<String>,
        preconditions: impl Into<String>,
    ) -> Result<Self, JournalError> {
        let spec = Self {
            action: action.into(),
            principal: principal.into(),
            target: target.into(),
            preconditions: preconditions.into(),
        };
        for (name, value) in [
            ("action", spec.action.as_str()),
            ("principal", spec.principal.as_str()),
            ("target", spec.target.as_str()),
            ("preconditions", spec.preconditions.as_str()),
        ] {
            if value.is_empty() && name != "preconditions" {
                return Err(JournalError::InvalidField(name));
            }
            if value.len() > MAX_FINGERPRINT_FIELD_BYTES {
                return Err(JournalError::InvalidField(name));
            }
        }
        Ok(spec)
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn preconditions(&self) -> &str {
        &self.preconditions
    }
}

impl EffectFingerprint {
    /// Length-prefixed SHA-256 so field boundaries cannot collide.
    pub fn compute(spec: &EffectSpec) -> Self {
        let mut hasher = Sha256::new();
        feed(&mut hasher, spec.action.as_bytes());
        feed(&mut hasher, spec.principal.as_bytes());
        feed(&mut hasher, spec.target.as_bytes());
        feed(&mut hasher, spec.preconditions.as_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    pub fn as_hex(&self) -> String {
        hex32(&self.0)
    }
}

impl OperationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Executing => "executing",
            Self::Committed => "committed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
            Self::Reconciled => "reconciled",
        }
    }
}

impl FromStr for OperationState {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "prepared" => Ok(Self::Prepared),
            "executing" => Ok(Self::Executing),
            "committed" => Ok(Self::Committed),
            "failed" => Ok(Self::Failed),
            "uncertain" => Ok(Self::Uncertain),
            "reconciled" => Ok(Self::Reconciled),
            _ => Err(()),
        }
    }
}

impl IdempotencyClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SafeReplay => "safe_replay",
            Self::AtMostOnce => "at_most_once",
        }
    }
}

impl FromStr for IdempotencyClass {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "safe_replay" => Ok(Self::SafeReplay),
            "at_most_once" => Ok(Self::AtMostOnce),
            _ => Err(()),
        }
    }
}

impl EgressOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for EgressOutcome {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "attempted" => Ok(Self::Attempted),
            "delivered" => Ok(Self::Delivered),
            "failed" => Ok(Self::Failed),
            _ => Err(()),
        }
    }
}

impl WaitState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

impl FromStr for WaitState {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "expired" => Ok(Self::Expired),
            _ => Err(()),
        }
    }
}

impl OperationRecord {
    pub fn id(&self) -> OperationId {
        self.id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn fingerprint(&self) -> EffectFingerprint {
        self.fingerprint
    }

    pub fn state(&self) -> OperationState {
        self.state
    }

    pub fn idempotency(&self) -> IdempotencyClass {
        self.idempotency
    }

    pub fn reconcile_ref(&self) -> Option<&str> {
        self.reconcile_ref.as_deref()
    }

    /// Recovery decision. At-most-once executing/uncertain work is never Replay.
    pub fn replay_policy(&self) -> ReplayPolicy {
        match (self.state, self.idempotency) {
            (OperationState::Prepared, _) => ReplayPolicy::Replay,
            (OperationState::Executing, IdempotencyClass::SafeReplay)
            | (OperationState::Uncertain, IdempotencyClass::SafeReplay) => ReplayPolicy::Replay,
            (OperationState::Executing, IdempotencyClass::AtMostOnce)
            | (OperationState::Uncertain, IdempotencyClass::AtMostOnce) => ReplayPolicy::Reconcile,
            (
                OperationState::Committed | OperationState::Failed | OperationState::Reconciled,
                _,
            ) => ReplayPolicy::None,
        }
    }
}

impl EgressReceipt {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn prev_hash(&self) -> &str {
        &self.prev_hash
    }

    pub fn receipt_hash(&self) -> &str {
        &self.receipt_hash
    }

    pub fn outcome(&self) -> EgressOutcome {
        self.outcome
    }
}

impl WaitRecord {
    pub fn wait_token(&self) -> &str {
        &self.wait_token
    }

    pub fn state(&self) -> WaitState {
        self.state
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
}

impl OperationJournal {
    pub fn new(ledger: EventLedger) -> Self {
        Self { ledger }
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    /// Insert a prepared operation. Effect has not started.
    pub fn prepare(
        &self,
        session_id: SessionId,
        spec: &EffectSpec,
        idempotency: IdempotencyClass,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        cancel.check()?;
        let fingerprint = EffectFingerprint::compute(spec);
        let id = EventId::new();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        ensure_session(&tx, session_id)?;
        let created_at = read_now(&tx)?;
        tx.execute(
            "INSERT INTO operation_journal (
                id, session_id, fingerprint, state, idempotency, reconcile_ref, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?6)",
            params![
                id.to_string(),
                session_id.to_string(),
                fingerprint.as_hex(),
                OperationState::Prepared.as_str(),
                idempotency.as_str(),
                created_at,
            ],
        )?;
        cancel.check()?;
        tx.commit()?;
        Ok(OperationRecord {
            id,
            session_id,
            fingerprint,
            state: OperationState::Prepared,
            idempotency,
            reconcile_ref: None,
        })
    }

    pub fn mark_executing(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        self.transition(id, OperationState::Executing, None, cancel)
    }

    pub fn commit(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        self.transition(id, OperationState::Committed, None, cancel)
    }

    pub fn fail(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        self.transition(id, OperationState::Failed, None, cancel)
    }

    pub fn mark_uncertain(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        self.transition(id, OperationState::Uncertain, None, cancel)
    }

    /// Terminal reconcile. At-most-once in-flight work must take this path.
    pub fn reconcile(
        &self,
        id: OperationId,
        reconcile_ref: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        let reference = reconcile_ref.into();
        if reference.is_empty() || reference.len() > MAX_FINGERPRINT_FIELD_BYTES {
            return Err(JournalError::InvalidField("reconcile_ref"));
        }
        self.transition(id, OperationState::Reconciled, Some(reference), cancel)
    }

    /// The most recent operation in `session` with this effect fingerprint,
    /// if any. An at-most-once effect is identified by its fingerprint, so
    /// this is how a repeated request is answered from the journal instead
    /// of being executed a second time (ADR 0021, publication step 6).
    pub fn find_by_fingerprint(
        &self,
        session_id: SessionId,
        fingerprint: EffectFingerprint,
        cancel: &CancellationToken,
    ) -> Result<Option<OperationRecord>, JournalError> {
        cancel.check()?;
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT id, state, idempotency, reconcile_ref
                 FROM operation_journal
                 WHERE session_id = ?1 AND fingerprint = ?2
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                params![session_id.to_string(), fingerprint.as_hex()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, state, class, reconcile_ref)) = row else {
            return Ok(None);
        };
        Ok(Some(OperationRecord {
            id: OperationId::from_str(&id)
                .map_err(|_| JournalError::Corrupt("invalid operation id"))?,
            session_id,
            fingerprint,
            state: OperationState::from_str(&state)
                .map_err(|_| JournalError::Corrupt("unknown operation state"))?,
            idempotency: IdempotencyClass::from_str(&class)
                .map_err(|_| JournalError::Corrupt("unknown idempotency class"))?,
            reconcile_ref,
        }))
    }

    /// Every operation in `session` that has not reached a terminal state —
    /// the work a restart inherits. `Prepared` means the effect never
    /// started; `Executing`/`Uncertain` mean it may have. Recovery uses this
    /// to settle in-flight work before anything new is attempted (GVS-008).
    pub fn pending(
        &self,
        session_id: SessionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<OperationRecord>, JournalError> {
        cancel.check()?;
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, fingerprint, state, idempotency, reconcile_ref
             FROM operation_journal
             WHERE session_id = ?1 AND state IN ('prepared', 'executing', 'uncertain')
             ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, fp, state, class, reconcile_ref) = row?;
            out.push(OperationRecord {
                id: OperationId::from_str(&id)
                    .map_err(|_| JournalError::Corrupt("invalid operation id"))?,
                session_id,
                fingerprint: parse_fingerprint(&fp)?,
                state: OperationState::from_str(&state)
                    .map_err(|_| JournalError::Corrupt("unknown operation state"))?,
                idempotency: IdempotencyClass::from_str(&class)
                    .map_err(|_| JournalError::Corrupt("unknown idempotency class"))?,
                reconcile_ref,
            });
        }
        Ok(out)
    }

    pub fn load(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        cancel.check()?;
        let conn = self.connect()?;
        load_row(&conn, id)?.ok_or(JournalError::NotFound { operation_id: id })
    }

    /// Host gate: at-most-once in-flight work cannot be re-executed.
    pub fn assert_replay_allowed(
        &self,
        id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        let record = self.load(id, cancel)?;
        match record.replay_policy() {
            ReplayPolicy::Replay => Ok(record),
            ReplayPolicy::Reconcile => Err(JournalError::ReplayForbidden),
            ReplayPolicy::None => Err(JournalError::InvalidTransition {
                from: record.state,
                to: record.state,
            }),
        }
    }

    /// Record an egress attempt. Always durable, including Failed outcomes.
    pub fn record_egress(
        &self,
        session_id: SessionId,
        operation_id: OperationId,
        outcome: EgressOutcome,
        cancel: &CancellationToken,
    ) -> Result<EgressReceipt, JournalError> {
        cancel.check()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        ensure_session(&tx, session_id)?;
        let _op = load_row(&tx, operation_id)?.ok_or(JournalError::NotFound { operation_id })?;
        let last: Option<(i64, String)> = tx
            .query_row(
                "SELECT attempt, receipt_hash FROM egress_receipts
                 WHERE session_id = ?1 AND operation_id = ?2
                 ORDER BY attempt DESC LIMIT 1",
                params![session_id.to_string(), operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (attempt, prev_hash) = match last {
            Some((n, hash)) => (u32::try_from(n + 1).unwrap_or(u32::MAX), hash),
            None => (1, GENESIS_HASH.to_owned()),
        };
        if attempt == u32::MAX {
            return Err(JournalError::Corrupt("egress attempt counter exhausted"));
        }
        let created_at = read_now(&tx)?;
        let receipt_hash = hash_receipt(&prev_hash, operation_id, attempt, outcome);
        tx.execute(
            "INSERT INTO egress_receipts (
                session_id, operation_id, attempt, prev_hash, receipt_hash, outcome, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.to_string(),
                operation_id.to_string(),
                attempt as i64,
                prev_hash,
                receipt_hash,
                outcome.as_str(),
                created_at,
            ],
        )?;
        cancel.check()?;
        tx.commit()?;
        Ok(EgressReceipt {
            session_id,
            operation_id,
            attempt,
            prev_hash,
            receipt_hash,
            outcome,
        })
    }

    pub fn egress_chain(
        &self,
        session_id: SessionId,
        operation_id: OperationId,
        cancel: &CancellationToken,
    ) -> Result<Vec<EgressReceipt>, JournalError> {
        cancel.check()?;
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT attempt, prev_hash, receipt_hash, outcome
             FROM egress_receipts
             WHERE session_id = ?1 AND operation_id = ?2
             ORDER BY attempt ASC",
        )?;
        let rows = stmt.query_map(
            params![session_id.to_string(), operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            cancel.check()?;
            let (attempt, prev_hash, receipt_hash, outcome) = row?;
            let outcome = EgressOutcome::from_str(&outcome)
                .map_err(|_| JournalError::Corrupt("unknown egress outcome"))?;
            out.push(EgressReceipt {
                session_id,
                operation_id,
                attempt: u32::try_from(attempt)
                    .map_err(|_| JournalError::Corrupt("negative egress attempt"))?,
                prev_hash,
                receipt_hash,
                outcome,
            });
        }
        Ok(out)
    }

    /// Create a pending wait. Duplicate tokens fail closed.
    pub fn request_wait(
        &self,
        session_id: SessionId,
        wait_token: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<WaitRecord, JournalError> {
        cancel.check()?;
        let wait_token = wait_token.into();
        validate_wait_token(&wait_token)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        ensure_session(&tx, session_id)?;
        let created_at = read_now(&tx)?;
        match tx.execute(
            "INSERT INTO approvals (
                session_id, wait_token, state, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, ?4, NULL)",
            params![
                session_id.to_string(),
                wait_token,
                WaitState::Pending.as_str(),
                created_at,
            ],
        ) {
            Ok(1) => {}
            Ok(_) => return Err(JournalError::Corrupt("wait insert row count")),
            Err(err) if is_constraint(&err) => return Err(JournalError::Conflict),
            Err(err) => return Err(err.into()),
        }
        cancel.check()?;
        tx.commit()?;
        Ok(WaitRecord {
            session_id,
            wait_token,
            state: WaitState::Pending,
        })
    }

    pub fn resolve_wait(
        &self,
        session_id: SessionId,
        wait_token: &str,
        state: WaitState,
        cancel: &CancellationToken,
    ) -> Result<WaitRecord, JournalError> {
        cancel.check()?;
        if !state.is_terminal() {
            return Err(JournalError::InvalidField("wait_state"));
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let current =
            load_wait(&tx, session_id, wait_token)?.ok_or_else(|| JournalError::WaitNotFound {
                wait_token: wait_token.to_owned(),
            })?;
        if current.state.is_terminal() {
            return Err(JournalError::Conflict);
        }
        let resolved_at = read_now(&tx)?;
        tx.execute(
            "UPDATE approvals SET state = ?1, resolved_at = ?2
             WHERE session_id = ?3 AND wait_token = ?4",
            params![
                state.as_str(),
                resolved_at,
                session_id.to_string(),
                wait_token,
            ],
        )?;
        cancel.check()?;
        tx.commit()?;
        Ok(WaitRecord {
            session_id,
            wait_token: wait_token.to_owned(),
            state,
        })
    }

    pub fn load_wait(
        &self,
        session_id: SessionId,
        wait_token: &str,
        cancel: &CancellationToken,
    ) -> Result<WaitRecord, JournalError> {
        cancel.check()?;
        let conn = self.connect()?;
        load_wait(&conn, session_id, wait_token)?.ok_or_else(|| JournalError::WaitNotFound {
            wait_token: wait_token.to_owned(),
        })
    }

    fn transition(
        &self,
        id: OperationId,
        to: OperationState,
        reconcile_ref: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<OperationRecord, JournalError> {
        cancel.check()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let mut record = load_row(&tx, id)?.ok_or(JournalError::NotFound { operation_id: id })?;
        if !legal_transition(record.state, to) {
            return Err(JournalError::InvalidTransition {
                from: record.state,
                to,
            });
        }
        let updated_at = read_now(&tx)?;
        tx.execute(
            "UPDATE operation_journal
             SET state = ?1, reconcile_ref = COALESCE(?2, reconcile_ref), updated_at = ?3
             WHERE id = ?4",
            params![
                to.as_str(),
                reconcile_ref.as_deref(),
                updated_at,
                id.to_string()
            ],
        )?;
        cancel.check()?;
        tx.commit()?;
        record.state = to;
        if reconcile_ref.is_some() {
            record.reconcile_ref = reconcile_ref;
        }
        Ok(record)
    }

    fn connect(&self) -> Result<Connection, JournalError> {
        let conn = Connection::open(self.ledger.path())?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", 1)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }
}

fn legal_transition(from: OperationState, to: OperationState) -> bool {
    matches!(
        (from, to),
        (OperationState::Prepared, OperationState::Executing)
            | (OperationState::Executing, OperationState::Committed)
            | (OperationState::Executing, OperationState::Failed)
            | (OperationState::Executing, OperationState::Uncertain)
            | (OperationState::Uncertain, OperationState::Reconciled)
    )
}

fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn parse_fingerprint(hex: &str) -> Result<EffectFingerprint, JournalError> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(JournalError::Corrupt("invalid fingerprint"));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| JournalError::Corrupt("invalid fingerprint"))?;
    }
    Ok(EffectFingerprint(out))
}

fn hash_receipt(
    prev_hash: &str,
    operation_id: OperationId,
    attempt: u32,
    outcome: EgressOutcome,
) -> String {
    let mut hasher = Sha256::new();
    feed(&mut hasher, prev_hash.as_bytes());
    feed(&mut hasher, operation_id.to_string().as_bytes());
    hasher.update(attempt.to_be_bytes());
    feed(&mut hasher, outcome.as_str().as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    hex32(&bytes)
}

fn ensure_session(conn: &Connection, session_id: SessionId) -> Result<(), JournalError> {
    let found = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    if found.is_some() {
        Ok(())
    } else {
        Err(JournalError::SessionNotFound { session_id })
    }
}

fn load_row(conn: &Connection, id: OperationId) -> Result<Option<OperationRecord>, JournalError> {
    let row = conn
        .query_row(
            "SELECT session_id, fingerprint, state, idempotency, reconcile_ref
             FROM operation_journal WHERE id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((session, fp, state, class, reconcile_ref)) = row else {
        return Ok(None);
    };
    Ok(Some(OperationRecord {
        id,
        session_id: SessionId::from_str(&session)
            .map_err(|_| JournalError::Corrupt("invalid session id"))?,
        fingerprint: parse_fingerprint(&fp)?,
        state: OperationState::from_str(&state)
            .map_err(|_| JournalError::Corrupt("unknown operation state"))?,
        idempotency: IdempotencyClass::from_str(&class)
            .map_err(|_| JournalError::Corrupt("unknown idempotency class"))?,
        reconcile_ref,
    }))
}

fn load_wait(
    conn: &Connection,
    session_id: SessionId,
    wait_token: &str,
) -> Result<Option<WaitRecord>, JournalError> {
    let row = conn
        .query_row(
            "SELECT state FROM approvals WHERE session_id = ?1 AND wait_token = ?2",
            params![session_id.to_string(), wait_token],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(state) = row else {
        return Ok(None);
    };
    Ok(Some(WaitRecord {
        session_id,
        wait_token: wait_token.to_owned(),
        state: WaitState::from_str(&state)
            .map_err(|_| JournalError::Corrupt("unknown wait state"))?,
    }))
}

fn validate_wait_token(token: &str) -> Result<(), JournalError> {
    if token.is_empty() || token.len() > MAX_WAIT_TOKEN_BYTES {
        return Err(JournalError::InvalidField("wait_token"));
    }
    if token.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(JournalError::InvalidField("wait_token"));
    }
    Ok(())
}

fn read_now(conn: &Connection) -> Result<String, JournalError> {
    let raw: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    Ok(raw)
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    )
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("journal operation cancelled"),
            Self::SessionNotFound { .. } => f.write_str("journal session not found"),
            Self::NotFound { .. } => f.write_str("operation not found"),
            Self::WaitNotFound { .. } => f.write_str("wait record not found"),
            Self::InvalidTransition { from, to } => {
                write!(
                    f,
                    "invalid journal transition {} → {}",
                    from.as_str(),
                    to.as_str()
                )
            }
            Self::ReplayForbidden => f.write_str("at-most-once effect cannot be replayed"),
            Self::InvalidField(name) => write!(f, "invalid journal field {name}"),
            Self::Conflict => f.write_str("journal conflict"),
            Self::Corrupt(reason) => write!(f, "journal corrupt ({reason})"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for JournalError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ProjectId;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempJournal {
        path: PathBuf,
        journal: OperationJournal,
        session: SessionId,
    }

    impl TempJournal {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-op-journal-{}-{seq}.sqlite",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let ledger = EventLedger::open(&path).expect("open ledger");
            let session = SessionId::new();
            ledger
                .create_session(
                    session,
                    ProjectId::new(),
                    &crate::ledger::CancellationToken::new(),
                )
                .expect("session");
            Self {
                path,
                journal: OperationJournal::new(ledger),
                session,
            }
        }
    }

    impl Drop for TempJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let mut wal = self.path.as_os_str().to_os_string();
            wal.push("-wal");
            let _ = std::fs::remove_file(&wal);
            let mut shm = self.path.as_os_str().to_os_string();
            shm.push("-shm");
            let _ = std::fs::remove_file(&shm);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn spec() -> EffectSpec {
        EffectSpec::new(
            "http.post",
            "agent-1",
            "https://example.test/hooks",
            "etag=1",
        )
        .expect("spec")
    }

    #[test]
    fn fingerprint_is_stable_and_field_order_matters() {
        let a = EffectFingerprint::compute(&spec());
        let b = EffectFingerprint::compute(&spec());
        assert_eq!(a, b);
        let swapped = EffectSpec::new(
            "agent-1",
            "http.post",
            "https://example.test/hooks",
            "etag=1",
        )
        .expect("swapped");
        assert_ne!(a, EffectFingerprint::compute(&swapped));
    }

    #[test]
    fn prepare_executing_commit_happy_path() {
        let tmp = TempJournal::create();
        let op = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::SafeReplay, &live())
            .expect("prepare");
        assert_eq!(op.state(), OperationState::Prepared);
        let op = tmp
            .journal
            .mark_executing(op.id(), &live())
            .expect("executing");
        assert_eq!(op.state(), OperationState::Executing);
        let op = tmp.journal.commit(op.id(), &live()).expect("commit");
        assert_eq!(op.state(), OperationState::Committed);
        assert_eq!(op.replay_policy(), ReplayPolicy::None);
    }

    #[test]
    fn invalid_transition_fails_closed() {
        let tmp = TempJournal::create();
        let op = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::AtMostOnce, &live())
            .expect("prepare");
        let err = tmp
            .journal
            .commit(op.id(), &live())
            .expect_err("skip executing");
        assert!(matches!(
            err,
            JournalError::InvalidTransition {
                from: OperationState::Prepared,
                to: OperationState::Committed
            }
        ));
        assert_eq!(
            tmp.journal
                .load(op.id(), &live())
                .expect("still prepared")
                .state(),
            OperationState::Prepared
        );
    }

    #[test]
    fn at_most_once_executing_must_reconcile_not_replay() {
        let tmp = TempJournal::create();
        let op = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::AtMostOnce, &live())
            .expect("prepare");
        let op = tmp
            .journal
            .mark_executing(op.id(), &live())
            .expect("executing");
        assert_eq!(op.replay_policy(), ReplayPolicy::Reconcile);
        let err = tmp
            .journal
            .assert_replay_allowed(op.id(), &live())
            .expect_err("must not replay");
        assert!(matches!(err, JournalError::ReplayForbidden));
        let op = tmp
            .journal
            .mark_uncertain(op.id(), &live())
            .expect("uncertain");
        assert_eq!(op.replay_policy(), ReplayPolicy::Reconcile);
        let op = tmp
            .journal
            .reconcile(op.id(), "inspected-remote-id:abc", &live())
            .expect("reconcile");
        assert_eq!(op.state(), OperationState::Reconciled);
        assert_eq!(op.reconcile_ref(), Some("inspected-remote-id:abc"));
        assert_eq!(op.replay_policy(), ReplayPolicy::None);
    }

    #[test]
    fn safe_replay_executing_may_replay() {
        let tmp = TempJournal::create();
        let op = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::SafeReplay, &live())
            .expect("prepare");
        let op = tmp
            .journal
            .mark_executing(op.id(), &live())
            .expect("executing");
        assert_eq!(op.replay_policy(), ReplayPolicy::Replay);
    }

    #[test]
    fn egress_receipts_are_hash_linked_including_failures() {
        let tmp = TempJournal::create();
        let op = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::AtMostOnce, &live())
            .expect("prepare");
        let first = tmp
            .journal
            .record_egress(tmp.session, op.id(), EgressOutcome::Attempted, &live())
            .expect("attempt");
        assert_eq!(first.prev_hash(), GENESIS_HASH);
        assert_eq!(first.attempt(), 1);
        let failed = tmp
            .journal
            .record_egress(tmp.session, op.id(), EgressOutcome::Failed, &live())
            .expect("failed still recorded");
        assert_eq!(failed.prev_hash(), first.receipt_hash());
        assert_eq!(failed.attempt(), 2);
        let chain = tmp
            .journal
            .egress_chain(tmp.session, op.id(), &live())
            .expect("chain");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].prev_hash(), chain[0].receipt_hash());
        let recomputed = hash_receipt(chain[0].receipt_hash(), op.id(), 2, EgressOutcome::Failed);
        assert_eq!(chain[1].receipt_hash(), recomputed);
    }

    #[test]
    fn wait_records_are_durable_and_terminal() {
        let tmp = TempJournal::create();
        let wait = tmp
            .journal
            .request_wait(tmp.session, "ask-user-1", &live())
            .expect("request");
        assert_eq!(wait.state(), WaitState::Pending);
        let resolved = tmp
            .journal
            .resolve_wait(tmp.session, "ask-user-1", WaitState::Approved, &live())
            .expect("approve");
        assert_eq!(resolved.state(), WaitState::Approved);
        let err = tmp
            .journal
            .resolve_wait(tmp.session, "ask-user-1", WaitState::Denied, &live())
            .expect_err("already terminal");
        assert!(matches!(err, JournalError::Conflict));
        let reopened = OperationJournal::new(EventLedger::open(&tmp.path).expect("reopen"));
        let loaded = reopened
            .load_wait(tmp.session, "ask-user-1", &live())
            .expect("durable");
        assert_eq!(loaded.state(), WaitState::Approved);
    }

    #[test]
    fn cancelled_prepare_is_not_acknowledged() {
        let tmp = TempJournal::create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .journal
            .prepare(tmp.session, &spec(), IdempotencyClass::SafeReplay, &cancel)
            .expect_err("cancelled");
        assert!(matches!(err, JournalError::Cancelled));
    }
}
