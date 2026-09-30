//! Transactional event append with monotonic per-session sequence.
//!
//! `append` allocates `seq` inside one IMMEDIATE SQLite transaction and
//! returns an envelope only after that transaction commits. `synchronous=FULL`
//! is set on every connection so a successful return is durable.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::{EventId, ProjectId, RedactionClass, SessionId, TraceId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;

use crate::event::{
    ActorRef, ErasedEventEnvelope, EventEnvelope, EventKind, EventKindParseError, RecordedAt,
};
use crate::migrations::{MigrationError, MigrationRunner};

/// Maximum UTF-8 bytes accepted in a serialized event payload.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Bounded SQLite lock wait. Writers serialise through IMMEDIATE
/// transactions with `synchronous=FULL`, so under contention an append
/// waits for every earlier writer's fsync; on a slow disk sixteen
/// concurrent writers exceeded five seconds (Windows CI, 2026-09-17,
/// `concurrent_append_produces_gap_free_unique_seqs`: "database is
/// locked"). An event ledger prefers a long wait to a lost append; thirty
/// seconds is still bounded, and a lock held that long is a stuck writer,
/// not contention.
const BUSY_TIMEOUT: Duration = Duration::from_millis(30_000);

/// File-backed event ledger. Connections are opened per operation so writers
/// serialize through SQLite IMMEDIATE transactions rather than a process lock.
#[derive(Clone, Debug)]
pub struct EventLedger {
    path: PathBuf,
    fail_before_commit: Arc<AtomicBool>,
}

/// Attribution and concurrency metadata for [`EventLedger::append`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendOptions {
    pub redaction: RedactionClass,
    pub trace_id: TraceId,
    pub expected_seq: Option<u64>,
}

/// Cooperative cancellation for ledger operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed failures for ledger append and session-row operations.
#[derive(Debug)]
pub enum LedgerError {
    Cancelled,
    SessionNotFound {
        session_id: SessionId,
    },
    SessionExists {
        session_id: SessionId,
    },
    SequenceConflict {
        session_id: SessionId,
        expected: u64,
        actual: u64,
    },
    SequenceExhausted,
    EventNotFound {
        session_id: SessionId,
        seq: u64,
    },
    PayloadBound {
        limit: usize,
        observed: usize,
    },
    /// Insert ran but the transaction did not commit; the event is not durable.
    NotCommitted,
    Corrupt(&'static str),
    ForeignKeysDisabled,
    InvalidTimestamp,
    Migration(MigrationError),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
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

    pub fn check(&self) -> Result<(), LedgerError> {
        if self.is_cancelled() {
            Err(LedgerError::Cancelled)
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

/// One row of the cross-session listing used by inspectors and the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub last_seq: u64,
    pub first_seen: String,
    /// `recorded_at` of this session's most recent event.
    ///
    /// Distinct from [`Self::first_seen`], and the one a "resume where I
    /// left off" default needs: a session created yesterday and worked in
    /// today is the one a user means, not whichever was created last.
    pub last_activity: String,
    /// A session background work runs in (a loop's fire, marked by an
    /// `automation.trigger_received`), not one a person worked in: never
    /// what "resume where I left off" means.
    pub background: bool,
    /// Which surface created the session (`headless`, `interactive`,
    /// `acp`, `daemon`, `workflow`), from its `session.created` record;
    /// `None` for a session recorded before origins were.
    pub origin: Option<String>,
    /// The session's title: the newest `session.renamed` event's, cleaned
    /// again here (a ledger row is not trusted to be clean). A projection
    /// of the events — nothing else stores it. `None` if never renamed.
    pub title: Option<String>,
}

/// Longest session title, in characters.
pub const MAX_SESSION_TITLE_CHARS: usize = 80;

/// A title fit to store and to show: control and invisible formatting
/// characters become spaces, whitespace runs collapse, the ends are
/// trimmed and it is cut to [`MAX_SESSION_TITLE_CHARS`]. `None` when
/// nothing is left.
pub fn clean_session_title(raw: &str) -> Option<String> {
    /// What the last character kept was, for the emoji sequences below.
    #[derive(Clone, Copy, PartialEq)]
    enum Prev {
        Other,
        /// A digit, `#` or `*`: the base of a keycap.
        KeycapBase,
        Emoji,
        Selector,
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut spaced = String::with_capacity(raw.len());
    let mut prev = Prev::Other;
    for (at, &ch) in chars.iter().enumerate() {
        let next = chars.get(at + 1).copied();
        // Variation selectors and the zero-width joiner are invisible, so
        // anywhere but inside an emoji they are hidden data. They are kept
        // only where an emoji sequence has them — one selector right after
        // an emoji, a joiner between two emoji, a keycap's `U+FE0F` — and
        // so cannot carry text through a word, a number or a letter.
        let kept = match ch {
            '\u{FE0E}' | '\u{FE0F}' => {
                prev == Prev::Emoji
                    || (prev == Prev::KeycapBase && ch == '\u{FE0F}' && next == Some('\u{20E3}'))
            }
            '\u{200D}' => {
                matches!(prev, Prev::Emoji | Prev::Selector) && next.is_some_and(is_emoji_base)
            }
            _ => false,
        };
        if kept {
            spaced.push(ch);
            prev = if ch == '\u{200D}' {
                Prev::Other
            } else {
                Prev::Selector
            };
            continue;
        }
        let out = {
            // Formatting characters that draw nothing: soft hyphen, Arabic
            // letter mark, Mongolian vowel separator, zero-width and bidi
            // controls, invisible operators, Hangul filler, variation
            // selectors, and the Unicode "tag" block (the usual channel for
            // text no one can see).
            let invisible = matches!(
                ch,
                '\u{00AD}'
                    | '\u{061C}'
                    | '\u{180E}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{206F}'
                    | '\u{3164}'
                    | '\u{FE00}'..='\u{FE0F}'
                    | '\u{FEFF}'
                    | '\u{E0000}'..='\u{E007F}'
                    | '\u{E0100}'..='\u{E01EF}'
            );
            if ch.is_control() || invisible {
                ' '
            } else {
                ch
            }
        };
        prev = if is_emoji_base(out) {
            Prev::Emoji
        } else if matches!(out, '0'..='9' | '#' | '*') {
            Prev::KeycapBase
        } else {
            Prev::Other
        };
        spaced.push(out);
    }
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed.chars().take(MAX_SESSION_TITLE_CHARS).collect();
    // A cut can land just after a joiner, which then joins nothing.
    let cut = cut.trim_end_matches('\u{200D}').trim_end().to_owned();
    (!cut.is_empty()).then_some(cut)
}

/// Whether `ch` is an emoji or symbol that can carry an emoji variation
/// selector or join into a sequence — the ranges of the Unicode emoji data
/// that a title can reasonably hold, not every non-ASCII character.
fn is_emoji_base(ch: char) -> bool {
    matches!(
        ch,
        '\u{00A9}'
            | '\u{00AE}'
            | '\u{203C}'
            | '\u{2049}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{2194}'..='\u{21AA}'
            | '\u{231A}'..='\u{23FF}'
            | '\u{24C2}'
            | '\u{25AA}'..='\u{25FE}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2934}'..='\u{2935}'
            | '\u{2B05}'..='\u{2B55}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}'
    )
}

impl EventLedger {
    /// Open (or create) a file-backed ledger and apply migrations.
    ///
    /// In-memory databases are rejected because they cannot enable WAL.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        MigrationRunner::apply(&conn)?;
        drop(conn);
        Ok(Self {
            path: path.to_path_buf(),
            fail_before_commit: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert the `sessions` row required by the events foreign key.
    ///
    /// Durable before success. Does not append `session.created`.
    pub fn create_session(
        &self,
        session_id: SessionId,
        project_id: ProjectId,
        cancel: &CancellationToken,
    ) -> Result<(), LedgerError> {
        cancel.check()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let created_at = read_recorded_at(&tx)?;
        match tx.execute(
            "INSERT INTO sessions (id, project_id, created_at, closed_at)
             VALUES (?1, ?2, ?3, NULL)",
            params![
                session_id.to_string(),
                project_id.to_string(),
                created_at.as_str()
            ],
        ) {
            Ok(1) => {}
            Ok(_) => {
                return Err(LedgerError::Corrupt(
                    "session insert did not affect one row",
                ));
            }
            Err(err) if is_constraint(&err) => {
                return Err(LedgerError::SessionExists { session_id });
            }
            Err(err) => return Err(err.into()),
        }
        cancel.check()?;
        self.check_fail_before_commit()?;
        tx.commit()?;
        Ok(())
    }

    /// Append one event. `seq` is allocated inside the same transaction that
    /// inserts the row; success is returned only after commit.
    pub fn append<P: Serialize>(
        &self,
        session: SessionId,
        actor: ActorRef,
        kind: EventKind,
        payload: P,
        options: &AppendOptions,
        cancel: &CancellationToken,
    ) -> Result<EventEnvelope<P>, LedgerError> {
        cancel.check()?;
        let encoded = EncodedEvent::new(&actor, &payload)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel.check()?;
        let inserted = insert_event(&tx, session, kind, &encoded, options)?;
        cancel.check()?;
        self.check_fail_before_commit()?;
        tx.commit()?;
        Ok(inserted.into_envelope(session, actor, kind, options, payload))
    }

    /// Highest committed `seq` for `session`, or `0` when the session has none.
    pub fn last_seq(
        &self,
        session: SessionId,
        cancel: &CancellationToken,
    ) -> Result<u64, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        last_seq_tx(&conn, session)
    }

    /// Load one committed event. Missing `(session, seq)` is [`LedgerError::EventNotFound`].
    /// Cross-session index for the sessions inspector: one row per session
    /// ever written, ordered by first activity.
    pub fn list_sessions(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<SessionSummary>, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            // Ordering is unchanged (`MIN(recorded_at)`, oldest first) so
            // `rapid sessions list` reads the same; `last_activity` is a new
            // column, not a new sort.
            // One pass over the events; the newest rename's row is then
            // fetched by its primary key, so a session that was never
            // renamed costs nothing more. A value that is not text (a
            // hand-edited row) is no origin and no title, not an error.
            "SELECT g.session_id, g.last_seq, g.first_seen, g.last_activity, g.background,
                    g.origin,
                    (SELECT CASE WHEN json_type(r.payload_json, '$.title') = 'text'
                                 THEN json_extract(r.payload_json, '$.title') END
                       FROM events r
                      WHERE r.session_id = g.session_id AND r.seq = g.rename_seq)
             FROM (SELECT session_id,
                          COALESCE(MAX(seq),0) AS last_seq,
                          MIN(recorded_at) AS first_seen,
                          MAX(recorded_at) AS last_activity,
                          MAX(kind = 'automation.trigger_received') AS background,
                          MAX(CASE WHEN kind = 'session.created'
                                    AND json_type(payload_json, '$.origin') = 'text'
                                   THEN json_extract(payload_json, '$.origin') END) AS origin,
                          MAX(CASE WHEN kind = 'session.renamed' THEN seq END) AS rename_seq
                     FROM events GROUP BY session_id) g
             ORDER BY g.first_seen",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (session_id, last_seq, first_seen, last_activity, background, origin, title) = row?;
            out.push(SessionSummary {
                session_id,
                last_seq: last_seq.max(0) as u64,
                first_seen,
                last_activity,
                background,
                origin,
                title: title.as_deref().and_then(clean_session_title),
            });
        }
        drop(stmt);
        drop(conn);
        Ok(out)
    }

    pub fn get(
        &self,
        session: SessionId,
        seq: u64,
        cancel: &CancellationToken,
    ) -> Result<ErasedEventEnvelope, LedgerError> {
        cancel.check()?;
        if seq == 0 || seq > i64::MAX as u64 {
            return Err(LedgerError::EventNotFound {
                session_id: session,
                seq,
            });
        }
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        let row = conn
            .query_row(
                "SELECT event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json
                 FROM events WHERE session_id = ?1 AND seq = ?2",
                params![session.to_string(), seq as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json)) =
            row
        else {
            return Err(LedgerError::EventNotFound {
                session_id: session,
                seq,
            });
        };
        envelope_from_row(
            session,
            seq,
            StoredEventRow {
                event_id,
                recorded_at,
                actor_json,
                trace_id,
                kind,
                redaction,
                payload_json,
            },
        )
    }

    /// Every event of `session` whose kind starts with `kind_prefix` (such
    /// as `"job."`), oldest first — one connection and one query, however
    /// long the session, where reading it through [`Self::get`] costs a
    /// connection per event. The rows are one consistent snapshot.
    /// The oldest (`last == false`) or newest event whose kind starts with
    /// `kind_prefix`, or `None` — one row read, however long the session.
    pub fn edge_event_of_kind(
        &self,
        session: SessionId,
        kind_prefix: &str,
        last: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<ErasedEventEnvelope>, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        let prefix_len = i64::try_from(kind_prefix.chars().count()).unwrap_or(i64::MAX);
        let sql = if last {
            "SELECT seq, event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json
             FROM events WHERE session_id = ?1 AND substr(kind, 1, ?2) = ?3
             ORDER BY seq DESC LIMIT 1"
        } else {
            "SELECT seq, event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json
             FROM events WHERE session_id = ?1 AND substr(kind, 1, ?2) = ?3
             ORDER BY seq LIMIT 1"
        };
        let row = conn
            .query_row(
                sql,
                params![session.to_string(), prefix_len, kind_prefix],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        StoredEventRow {
                            event_id: row.get(1)?,
                            recorded_at: row.get(2)?,
                            actor_json: row.get(3)?,
                            trace_id: row.get(4)?,
                            kind: row.get(5)?,
                            redaction: row.get(6)?,
                            payload_json: row.get(7)?,
                        },
                    ))
                },
            )
            .optional()?;
        let Some((seq, row)) = row else {
            return Ok(None);
        };
        let seq = u64::try_from(seq).map_err(|_| LedgerError::EventNotFound {
            session_id: session,
            seq: 0,
        })?;
        Ok(Some(envelope_from_row(session, seq, row)?))
    }

    /// How many events of `session` have a kind starting with `kind_prefix`.
    pub fn count_of_kind(
        &self,
        session: SessionId,
        kind_prefix: &str,
        cancel: &CancellationToken,
    ) -> Result<u64, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        let prefix_len = i64::try_from(kind_prefix.chars().count()).unwrap_or(i64::MAX);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND substr(kind, 1, ?2) = ?3",
            params![session.to_string(), prefix_len, kind_prefix],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    pub fn events_of_kind(
        &self,
        session: SessionId,
        kind_prefix: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<ErasedEventEnvelope>, LedgerError> {
        cancel.check()?;
        let conn = self.connect()?;
        ensure_session(&conn, session)?;
        // The kind's first characters (`substr` counts characters, not
        // bytes), compared exactly: no wildcard in the prefix means
        // anything, and case counts (`LIKE` would ignore it).
        let prefix_len = i64::try_from(kind_prefix.chars().count()).unwrap_or(i64::MAX);
        let mut statement = conn.prepare(
            "SELECT seq, event_id, recorded_at, actor_json, trace_id, kind, redaction, payload_json
             FROM events WHERE session_id = ?1 AND substr(kind, 1, ?2) = ?3 ORDER BY seq",
        )?;
        let rows = statement.query_map(
            params![session.to_string(), prefix_len, kind_prefix],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    StoredEventRow {
                        event_id: row.get(1)?,
                        recorded_at: row.get(2)?,
                        actor_json: row.get(3)?,
                        trace_id: row.get(4)?,
                        kind: row.get(5)?,
                        redaction: row.get(6)?,
                        payload_json: row.get(7)?,
                    },
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            cancel.check()?;
            let (seq, row) = row?;
            let seq = u64::try_from(seq).map_err(|_| LedgerError::EventNotFound {
                session_id: session,
                seq: 0,
            })?;
            out.push(envelope_from_row(session, seq, row)?);
        }
        Ok(out)
    }

    /// Arm a one-shot rollback after a successful INSERT and before COMMIT.
    ///
    /// Used to prove that a pre-commit failure cannot acknowledge an event.
    #[cfg(test)]
    pub fn inject_fail_before_commit(&self) {
        self.fail_before_commit.store(true, Ordering::SeqCst);
    }

    /// Consume an armed [`Self::inject_fail_before_commit`]: the caller drops
    /// its transaction uncommitted.
    pub(crate) fn check_fail_before_commit(&self) -> Result<(), LedgerError> {
        if self.fail_before_commit.swap(false, Ordering::SeqCst) {
            return Err(LedgerError::NotCommitted);
        }
        Ok(())
    }

    /// A connection configured like every ledger writer's. Crate writers that
    /// commit an event together with another row open their transaction here.
    pub(crate) fn connect(&self) -> Result<Connection, LedgerError> {
        let conn = Connection::open(&self.path)?;
        configure_connection(&conn)?;
        Ok(conn)
    }
}

fn configure_connection(conn: &Connection) -> Result<(), LedgerError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", 1)?;
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(LedgerError::ForeignKeysDisabled);
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn ensure_session(conn: &Connection, session_id: SessionId) -> Result<(), LedgerError> {
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
        Err(LedgerError::SessionNotFound { session_id })
    }
}

/// An event's actor and payload, serialized and bound-checked before any
/// write lock is taken.
pub(crate) struct EncodedEvent {
    actor_json: String,
    payload_json: String,
}

impl EncodedEvent {
    pub(crate) fn new<P: Serialize>(actor: &ActorRef, payload: &P) -> Result<Self, LedgerError> {
        let payload_json = serialize_payload(payload)?;
        let actor_json = serde_json::to_string(actor)?;
        Ok(Self {
            actor_json,
            payload_json,
        })
    }
}

/// Where [`insert_event`] placed a row, pending the caller's commit.
pub(crate) struct InsertedEvent {
    event_id: EventId,
    seq: u64,
    recorded_at: RecordedAt,
}

impl InsertedEvent {
    pub(crate) fn into_envelope<P>(
        self,
        session: SessionId,
        actor: ActorRef,
        kind: EventKind,
        options: &AppendOptions,
        payload: P,
    ) -> EventEnvelope<P> {
        EventEnvelope::new(
            self.event_id,
            session,
            self.seq,
            self.recorded_at,
            actor,
            options.trace_id,
            kind,
            options.redaction,
            payload,
        )
    }
}

/// Allocate the session's next `seq` (enforcing `options.expected_seq`) and
/// insert one event row on `conn`, which holds an IMMEDIATE transaction.
/// Nothing is durable until the caller commits, so a writer that must land
/// another row with the event commits both or neither.
pub(crate) fn insert_event(
    conn: &Connection,
    session: SessionId,
    kind: EventKind,
    encoded: &EncodedEvent,
    options: &AppendOptions,
) -> Result<InsertedEvent, LedgerError> {
    ensure_session(conn, session)?;
    let last = last_seq_tx(conn, session)?;
    match options.expected_seq {
        Some(expected) if last != expected => {
            return Err(LedgerError::SequenceConflict {
                session_id: session,
                expected,
                actual: last,
            });
        }
        _ => {}
    }
    let seq = next_seq(last)?;
    let event_id = EventId::new();
    let recorded_at = read_recorded_at(conn)?;
    match conn.execute(
        "INSERT INTO events (
            session_id, seq, event_id, recorded_at, actor_json,
            trace_id, kind, redaction, payload_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            session.to_string(),
            seq as i64,
            event_id.to_string(),
            recorded_at.as_str(),
            encoded.actor_json,
            options.trace_id.to_string(),
            kind.as_str(),
            options.redaction.as_str(),
            encoded.payload_json,
        ],
    ) {
        Ok(1) => {}
        Ok(_) => return Err(LedgerError::Corrupt("event insert did not affect one row")),
        Err(err) => return Err(err.into()),
    }
    Ok(InsertedEvent {
        event_id,
        seq,
        recorded_at,
    })
}

fn last_seq_tx(conn: &Connection, session_id: SessionId) -> Result<u64, LedgerError> {
    let last: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    if last < 0 {
        return Err(LedgerError::Corrupt("negative event seq"));
    }
    Ok(last as u64)
}

fn next_seq(last: u64) -> Result<u64, LedgerError> {
    let next = last.checked_add(1).ok_or(LedgerError::SequenceExhausted)?;
    if next > i64::MAX as u64 {
        return Err(LedgerError::SequenceExhausted);
    }
    Ok(next)
}

fn read_recorded_at(conn: &Connection) -> Result<RecordedAt, LedgerError> {
    let raw: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    raw.parse().map_err(|_| LedgerError::InvalidTimestamp)
}

fn serialize_payload<P: Serialize>(payload: &P) -> Result<String, LedgerError> {
    let payload_json = serde_json::to_string(payload)?;
    let observed = payload_json.len();
    if observed > MAX_PAYLOAD_BYTES {
        return Err(LedgerError::PayloadBound {
            limit: MAX_PAYLOAD_BYTES,
            observed,
        });
    }
    Ok(payload_json)
}

struct StoredEventRow {
    event_id: String,
    recorded_at: String,
    actor_json: String,
    trace_id: String,
    kind: String,
    redaction: String,
    payload_json: String,
}

fn envelope_from_row(
    session_id: SessionId,
    seq: u64,
    row: StoredEventRow,
) -> Result<ErasedEventEnvelope, LedgerError> {
    let event_id: EventId = row
        .event_id
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored event_id"))?;
    let recorded_at: RecordedAt = row
        .recorded_at
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored recorded_at"))?;
    let actor: ActorRef = serde_json::from_str(&row.actor_json)
        .map_err(|_| LedgerError::Corrupt("malformed stored actor_json"))?;
    let trace_id: TraceId = row
        .trace_id
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored trace_id"))?;
    let kind: EventKind = row
        .kind
        .parse()
        .map_err(|_: EventKindParseError| LedgerError::Corrupt("malformed stored kind"))?;
    let redaction: RedactionClass = row
        .redaction
        .parse()
        .map_err(|_| LedgerError::Corrupt("malformed stored redaction"))?;
    let payload: Value = serde_json::from_str(&row.payload_json)
        .map_err(|_| LedgerError::Corrupt("malformed stored payload_json"))?;
    Ok(EventEnvelope::new(
        event_id,
        session_id,
        seq,
        recorded_at,
        actor,
        trace_id,
        kind,
        redaction,
        payload,
    ))
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    )
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("ledger operation cancelled"),
            Self::SessionNotFound { session_id } => {
                write!(f, "session {session_id} not found")
            }
            Self::SessionExists { session_id } => {
                write!(f, "session {session_id} already exists")
            }
            Self::SequenceConflict {
                session_id,
                expected,
                actual,
            } => write!(
                f,
                "session {session_id} sequence conflict: expected {expected}, actual {actual}"
            ),
            Self::SequenceExhausted => f.write_str("session event sequence is exhausted"),
            Self::EventNotFound { session_id, seq } => {
                write!(f, "event {session_id}#{seq} not found")
            }
            Self::PayloadBound { limit, observed } => {
                write!(
                    f,
                    "event payload exceeds bound {limit}, observed {observed}"
                )
            }
            Self::NotCommitted => {
                f.write_str("event insert was not committed and must not be acknowledged")
            }
            Self::Corrupt(reason) => write!(f, "ledger data is corrupt: {reason}"),
            Self::ForeignKeysDisabled => f.write_str("sqlite foreign_keys pragma is disabled"),
            Self::InvalidTimestamp => f.write_str("sqlite produced a non-canonical recorded_at"),
            Self::Migration(err) => write!(f, "ledger migration error: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(err) => write!(f, "event json error: {err}"),
            Self::Io(err) => write!(f, "ledger io error: {err}"),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Migration(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Cancelled
            | Self::SessionNotFound { .. }
            | Self::SessionExists { .. }
            | Self::SequenceConflict { .. }
            | Self::SequenceExhausted
            | Self::EventNotFound { .. }
            | Self::PayloadBound { .. }
            | Self::NotCommitted
            | Self::Corrupt(_)
            | Self::ForeignKeysDisabled
            | Self::InvalidTimestamp => None,
        }
    }
}

impl From<MigrationError> for LedgerError {
    fn from(value: MigrationError) -> Self {
        Self::Migration(value)
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for LedgerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_title_is_made_inert_and_bounded() {
        use super::{MAX_SESSION_TITLE_CHARS, clean_session_title};
        assert_eq!(
            clean_session_title("  fix \t the\n parser  ").as_deref(),
            Some("fix the parser")
        );
        // Escapes, NULs and bidi/zero-width controls become spaces.
        assert_eq!(
            clean_session_title("a\u{1b}[2Jb\u{202e}c\u{200b}d\0e").as_deref(),
            Some("a [2Jb c d e")
        );
        assert_eq!(clean_session_title(" \n\u{200b}\u{202e} "), None);
        // Text no one can see — tag characters, variation selectors, the
        // soft hyphen, Hangul filler — leaves nothing behind.
        assert_eq!(
            clean_session_title("a\u{E0041}\u{E0042}b\u{FE0F}c\u{00AD}d\u{3164}e").as_deref(),
            Some("a b c d e")
        );
        assert_eq!(clean_session_title("\u{E0041}\u{E0042}\u{FE0F}"), None);
        // Ordinary emoji are left whole: variation selectors, joiners,
        // keycaps.
        for whole in [
            "\u{26A0}\u{FE0F} deploy",
            "\u{2764}\u{FE0F}x",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} plan",
            "1\u{FE0F}\u{20E3} first",
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308} flag",
        ] {
            assert_eq!(
                clean_session_title(whole).as_deref(),
                Some(whole),
                "{whole}"
            );
        }
        // Anywhere else they are hidden data and go: after letters
        // (Latin or not), inside a number, in a run, before a non-emoji.
        assert_eq!(clean_session_title("a\u{FE0F}b").as_deref(), Some("a b"));
        assert_eq!(clean_session_title("a\u{200D}b").as_deref(), Some("a b"));
        assert_eq!(
            clean_session_title("\u{E9}\u{FE0F}\u{200D}\u{E9}\u{65E5}\u{FE0E}").as_deref(),
            Some("\u{E9} \u{E9}\u{65E5}")
        );
        assert_eq!(
            clean_session_title("1\u{200D}2\u{200D}3\u{FE0E}4\u{FE0F}5").as_deref(),
            Some("1 2 3 4 5")
        );
        let run =
            clean_session_title("\u{2764}\u{FE0F}\u{FE0F}\u{FE0F}\u{FE0F}x").expect("a title");
        assert_eq!(run.matches('\u{FE0F}').count(), 1, "{run:?}");
        // A joiner must join two emoji: not before text, not at the end.
        assert_eq!(
            clean_session_title("\u{1F600}\u{200D} tail").as_deref(),
            Some("\u{1F600} tail")
        );
        assert_eq!(
            clean_session_title("\u{1F600}\u{200D}").as_deref(),
            Some("\u{1F600}")
        );
        // A cut that lands after a joiner leaves none dangling.
        let cut = clean_session_title(&format!(
            "{}\u{1F468}\u{200D}\u{1F469}",
            "x".repeat(MAX_SESSION_TITLE_CHARS - 2)
        ))
        .expect("a title");
        assert!(cut.ends_with('\u{1F468}'), "{cut:?}");
        assert_eq!(clean_session_title(""), None);
        let long = "x".repeat(MAX_SESSION_TITLE_CHARS + 50);
        assert_eq!(
            clean_session_title(&long).map(|t| t.chars().count()),
            Some(MAX_SESSION_TITLE_CHARS)
        );
        // Cut on characters, never inside one.
        let wide = "é".repeat(MAX_SESSION_TITLE_CHARS + 5);
        assert_eq!(
            clean_session_title(&wide).map(|t| t.chars().count()),
            Some(MAX_SESSION_TITLE_CHARS)
        );
    }

    use super::*;
    use crate::event::ActorKind;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempLedger {
        path: PathBuf,
        ledger: EventLedger,
    }

    impl TempLedger {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-event-ledger-{}-{seq}.sqlite",
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

    fn committed_seqs(ledger: &EventLedger, session: SessionId) -> Vec<u64> {
        let last = ledger.last_seq(session, &live()).expect("last_seq");
        (1..=last)
            .map(|seq| {
                let event = ledger.get(session, seq, &live()).expect("get committed");
                event.seq()
            })
            .collect()
    }

    #[test]
    fn append_allocates_monotonic_seq_inside_transaction() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);

        let first = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::SessionCreated,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(0),
                    ..options()
                },
                &live(),
            )
            .expect("first");
        assert_eq!(first.seq(), 1);
        assert_eq!(first.session_id(), session);
        assert_eq!(first.kind(), EventKind::SessionCreated);

        let second = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({"k":"v"}),
                &options(),
                &live(),
            )
            .expect("second");
        assert_eq!(second.seq(), 2);
        assert_eq!(committed_seqs(&tmp.ledger, session), vec![1, 2]);
    }

    #[test]
    fn events_of_kind_reads_one_kind_of_one_session_in_order() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let other = seed_session(&tmp.ledger);
        let append = |session: SessionId, kind: EventKind| {
            tmp.ledger
                .append(
                    session,
                    actor(),
                    kind,
                    serde_json::json!({}),
                    &options(),
                    &live(),
                )
                .expect("append")
                .seq()
        };
        append(session, EventKind::SessionCreated);
        let started = append(session, EventKind::JobStarted);
        append(session, EventKind::TurnStarted);
        let completed = append(session, EventKind::JobCompleted);
        append(other, EventKind::JobStarted);

        let jobs = tmp
            .ledger
            .events_of_kind(session, "job.", &live())
            .expect("read");
        assert_eq!(
            jobs.iter()
                .map(|event| (event.seq(), event.kind()))
                .collect::<Vec<_>>(),
            vec![
                (started, EventKind::JobStarted),
                (completed, EventKind::JobCompleted)
            ]
        );
        // The prefix is literal and exact: `_` is not a one-character
        // wildcard, so `turn_` does not match `turn.started`, and case
        // counts.
        for prefix in ["turn_", "JOB."] {
            assert!(
                tmp.ledger
                    .events_of_kind(session, prefix, &live())
                    .expect("read")
                    .is_empty(),
                "{prefix}"
            );
        }
        assert!(
            tmp.ledger
                .events_of_kind(SessionId::new(), "job.", &live())
                .is_err(),
            "an unknown session is an error, not an empty history"
        );
    }

    #[test]
    fn concurrent_append_produces_gap_free_unique_seqs() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let n_threads = 16;
        let per_thread = 4;
        let barrier = Arc::new(Barrier::new(n_threads));
        let mut handles = Vec::with_capacity(n_threads);
        for t in 0..n_threads {
            let ledger = tmp.ledger.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let mut seqs = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let envelope = ledger
                        .append(
                            session,
                            actor(),
                            EventKind::TurnStarted,
                            serde_json::json!({ "t": t, "i": i }),
                            &options(),
                            &live(),
                        )
                        .expect("concurrent append");
                    seqs.push(envelope.seq());
                }
                seqs
            }));
        }

        let mut seen = BTreeSet::new();
        for handle in handles {
            for seq in handle.join().expect("thread") {
                assert!(seen.insert(seq), "duplicate seq {seq}");
            }
        }
        let expected: BTreeSet<u64> = (1..=(n_threads * per_thread) as u64).collect();
        assert_eq!(seen, expected);
        assert_eq!(
            committed_seqs(&tmp.ledger, session),
            expected.into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn fail_before_commit_does_not_acknowledge_uncommitted_event() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        tmp.ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({"ok": true}),
                &options(),
                &live(),
            )
            .expect("committed predecessor");

        tmp.ledger.inject_fail_before_commit();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnCompleted,
                serde_json::json!({"ack": "must-not-happen"}),
                &options(),
                &live(),
            )
            .expect_err("injected pre-commit failure");
        assert!(
            matches!(err, LedgerError::NotCommitted),
            "acknowledged uncommitted event: {err}"
        );
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("last"), 1);
        let err = tmp
            .ledger
            .get(session, 2, &live())
            .expect_err("seq 2 must be absent");
        assert!(matches!(err, LedgerError::EventNotFound { seq: 2, .. }));

        let reopened = EventLedger::open(&tmp.path).expect("reopen after injected failure");
        assert_eq!(reopened.last_seq(session, &live()).expect("reopen last"), 1);
        let surviving = reopened.get(session, 1, &live()).expect("seq 1 durable");
        assert_eq!(surviving.kind(), EventKind::TurnStarted);
        let err = reopened
            .get(session, 2, &live())
            .expect_err("reopen must not see uncommitted");
        assert!(matches!(err, LedgerError::EventNotFound { seq: 2, .. }));
    }

    #[test]
    fn successful_append_is_durable_after_reopen() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let written = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::ToolCompleted,
                serde_json::json!({"status":"ok"}),
                &options(),
                &live(),
            )
            .expect("append");
        assert_eq!(written.seq(), 1);

        let reopened = EventLedger::open(&tmp.path).expect("reopen");
        let loaded = reopened.get(session, 1, &live()).expect("reload");
        assert_eq!(loaded.event_id(), written.event_id());
        assert_eq!(loaded.seq(), 1);
        assert_eq!(loaded.kind(), EventKind::ToolCompleted);
        assert_eq!(loaded.redaction(), RedactionClass::Project);
        assert_eq!(loaded.payload()["status"], "ok");
        assert_eq!(reopened.last_seq(session, &live()).expect("last"), 1);
    }

    #[test]
    fn expected_seq_conflict_does_not_append() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        append_n(&tmp.ledger, session, 1);
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(0),
                    ..options()
                },
                &live(),
            )
            .expect_err("stale expected_seq");
        match err {
            LedgerError::SequenceConflict {
                session_id,
                expected,
                actual,
            } => {
                assert_eq!(session_id, session);
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            other => panic!("expected SequenceConflict, got {other}"),
        }
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("unchanged"), 1);

        let next = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnCompleted,
                serde_json::json!({}),
                &AppendOptions {
                    expected_seq: Some(1),
                    ..options()
                },
                &live(),
            )
            .expect("matching expected_seq");
        assert_eq!(next.seq(), 2);
    }

    #[test]
    fn missing_session_is_not_appended() {
        let tmp = TempLedger::create();
        let session = SessionId::new();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &options(),
                &live(),
            )
            .expect_err("missing session");
        assert!(matches!(
            err,
            LedgerError::SessionNotFound { session_id } if session_id == session
        ));
    }

    #[test]
    fn cancelled_append_is_not_acknowledged() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::TurnStarted,
                serde_json::json!({}),
                &options(),
                &cancel,
            )
            .expect_err("cancelled");
        assert!(matches!(err, LedgerError::Cancelled));
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);
    }

    #[test]
    fn payload_bound_is_enforced() {
        let tmp = TempLedger::create();
        let session = seed_session(&tmp.ledger);
        let blob = "x".repeat(MAX_PAYLOAD_BYTES);
        let err = tmp
            .ledger
            .append(
                session,
                actor(),
                EventKind::JobOutput,
                serde_json::json!({ "blob": blob }),
                &options(),
                &live(),
            )
            .expect_err("bound");
        assert!(
            matches!(err, LedgerError::PayloadBound { limit, .. } if limit == MAX_PAYLOAD_BYTES),
            "got {err}"
        );
        assert_eq!(tmp.ledger.last_seq(session, &live()).expect("empty"), 0);
    }

    #[test]
    fn a_session_with_a_trigger_is_listed_as_background_work() {
        let tmp = TempLedger::create();
        let person = seed_session(&tmp.ledger);
        let fired = seed_session(&tmp.ledger);
        for (session, kind) in [
            (person, EventKind::TurnStarted),
            (fired, EventKind::AutomationTriggerReceived),
            (fired, EventKind::TurnStarted),
        ] {
            tmp.ledger
                .append(
                    session,
                    actor(),
                    kind,
                    serde_json::json!({}),
                    &options(),
                    &live(),
                )
                .expect("append");
        }
        let listed: std::collections::HashMap<String, bool> = tmp
            .ledger
            .list_sessions(&live())
            .expect("list")
            .into_iter()
            .map(|summary| (summary.session_id, summary.background))
            .collect();
        assert_eq!(listed.get(&person.to_string()), Some(&false));
        assert_eq!(listed.get(&fired.to_string()), Some(&true));
    }

    #[test]
    fn sessions_have_independent_sequences() {
        let tmp = TempLedger::create();
        let a = seed_session(&tmp.ledger);
        let b = seed_session(&tmp.ledger);
        assert_eq!(append_n(&tmp.ledger, a, 2), vec![1, 2]);
        assert_eq!(append_n(&tmp.ledger, b, 1), vec![1]);
        assert_eq!(tmp.ledger.last_seq(a, &live()).expect("a"), 2);
        assert_eq!(tmp.ledger.last_seq(b, &live()).expect("b"), 1);
    }

    #[test]
    fn duplicate_session_row_is_rejected() {
        let tmp = TempLedger::create();
        let session = SessionId::new();
        let project = ProjectId::new();
        tmp.ledger
            .create_session(session, project, &live())
            .expect("first");
        let err = tmp
            .ledger
            .create_session(session, project, &live())
            .expect_err("duplicate");
        assert!(matches!(
            err,
            LedgerError::SessionExists { session_id } if session_id == session
        ));
    }
}
