//! Subagent operations hygiene: attempt selection, worktree sweep planning,
//! and replay hydration of child results.
//!
//! Three pure decision surfaces used by hosts that run children:
//!
//! * [`AttemptSelector`] picks the winner of a best-of-N retry set by typed
//!   score; cancelled/failed attempts are rejected outright and an all-rejected
//!   set is a typed error, never a silent fallback to a failed child.
//! * [`SweepPlanner`] decides which tracked worktree records an operator or
//!   supervisor should remove. Planning is pure and idempotent — running it
//!   again after removals are applied plans nothing new — and it never asks
//!   for the removal of an active view.
//! * [`replay`] hydrates child attempts from persisted records after a crash
//!   so the parent can re-select instead of re-running children.

use std::fmt;

use workspace::GitWorktreeRecordState;

use super::model::AgentTerminalStatus;
use protocol::WorkspaceViewId;

/// Maximum attempts one best-of-N selection may consider.
pub const MAX_ATTEMPTS: usize = 8;

/// Wire identity of the replay record format.
pub const REPLAY_SCHEMA: &str = "rapidlm.subagent.replay.v1";

/// Maximum UTF-8 bytes in a replayed attempt summary.
pub const MAX_REPLAY_SUMMARY_BYTES: usize = 8 * 1024;
/// Maximum replay records hydrated in one pass.
pub const MAX_REPLAY_RECORDS: usize = 128;

/// One observed child attempt, as scored by the host after its turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoredAttempt {
    /// Zero-based attempt index; ties resolve to the lowest index.
    pub index: usize,
    pub status: AgentTerminalStatus,
    /// Tokens the attempt consumed (observation, not a promise).
    pub tokens: u64,
}

/// Why an attempt can never win.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptRejection {
    Cancelled,
    Failed,
}

impl AttemptRejection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// Typed attempt-selection failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptSelectionError {
    /// Attempt indexes must be unique; duplicates mean the host double-counted.
    DuplicateIndex,
    /// More attempts than one selection may consider.
    TooManyAttempts {
        limit: usize,
    },
    /// Every attempt was cancelled or failed.
    NoViableAttempt,
    /// An empty attempt list has nothing to select.
    NoAttempts,
}

impl fmt::Display for AttemptSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateIndex => write!(f, "attempt indexes must be unique"),
            Self::TooManyAttempts { limit } => {
                write!(f, "more than {limit} attempts in one selection")
            }
            Self::NoViableAttempt => write!(
                f,
                "every attempt was cancelled or failed; nothing is selectable"
            ),
            Self::NoAttempts => write!(f, "no attempts to select from"),
        }
    }
}

impl std::error::Error for AttemptSelectionError {}

/// Deterministic best-of-N selection policy.
pub struct AttemptSelector;

impl AttemptSelector {
    /// Reject terminal-failure attempts outright, then prefer the cheapest
    /// viable attempt by tokens; ties resolve to the lowest index. The
    /// decision depends only on the input list, so re-running it is stable.
    pub fn select_best(attempts: &[ScoredAttempt]) -> Result<&ScoredAttempt, AttemptSelectionError> {
        if attempts.is_empty() {
            return Err(AttemptSelectionError::NoAttempts);
        }
        if attempts.len() > MAX_ATTEMPTS {
            return Err(AttemptSelectionError::TooManyAttempts {
                limit: MAX_ATTEMPTS,
            });
        }
        for (i, attempt) in attempts.iter().enumerate() {
            if attempts[..i].iter().any(|prior| prior.index == attempt.index) {
                return Err(AttemptSelectionError::DuplicateIndex);
            }
        }
        attempts
            .iter()
            .filter(|attempt| {
                !matches!(
                    attempt.status,
                    AgentTerminalStatus::Failed | AgentTerminalStatus::Cancelled
                )
            })
            .min_by(|a, b| a.tokens.cmp(&b.tokens).then(a.index.cmp(&b.index)))
            .ok_or(AttemptSelectionError::NoViableAttempt)
    }

    /// The rejection reason for a non-viable attempt, if any.
    pub fn rejection_of(attempt: &ScoredAttempt) -> Option<AttemptRejection> {
        match attempt.status {
            AgentTerminalStatus::Failed => Some(AttemptRejection::Failed),
            AgentTerminalStatus::Cancelled => Some(AttemptRejection::Cancelled),
            AgentTerminalStatus::Succeeded => None,
        }
    }
}

/// One tracked worktree record as the store persists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepRecord {
    pub view_id: WorkspaceViewId,
    pub state: GitWorktreeRecordState,
}

/// Why a record is kept, not removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainReason {
    /// A `git worktree remove` was already attempted and failed; an operator
    /// must inspect the record before another pass.
    CleanupFailedNeedsOperator,
    /// The view is active and may be in use.
    Active,
}

/// Worktree cleanup decision for one sweep pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// Records safe to remove: abandoned allocations stuck mid-creation.
    pub remove: Vec<WorkspaceViewId>,
    /// Records to keep, with the reason.
    pub retain: Vec<(WorkspaceViewId, RetainReason)>,
}

/// Pure sweep planning over tracked record states.
pub struct SweepPlanner;

impl SweepPlanner {
    /// Decide removals for one pass. Rules:
    ///
    /// * `Creating` records are abandoned allocations (a crash between
    ///   allocation and activation) — plan removal;
    /// * `Active` records are retained — the view may be serving a child;
    /// * `CleanupFailed` records are retained for operator attention —
    ///   repeating a failed removal blindly is not hygiene.
    ///
    /// The plan is a pure function of the records: sweeping, applying
    /// removals through the store, and planning again produces only
    /// `retain` entries. No state is mutated here.
    pub fn plan(records: &[SweepRecord]) -> SweepPlan {
        let mut plan = SweepPlan::default();
        for record in records {
            match record.state {
                GitWorktreeRecordState::Creating => plan.remove.push(record.view_id),
                GitWorktreeRecordState::Active => {
                    plan.retain.push((record.view_id, RetainReason::Active));
                }
                GitWorktreeRecordState::CleanupFailed => {
                    plan.retain.push((
                        record.view_id,
                        RetainReason::CleanupFailedNeedsOperator,
                    ));
                }
            }
        }
        plan
    }
}

/// One replayable child attempt record. The host persists these after each
/// child turn so a crashed parent can hydrate and re-select without
/// re-running children.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayRecord {
    pub schema: String,
    /// Attempt index the host assigned when the child ran.
    pub index: usize,
    pub status: String,
    pub tokens: u64,
    pub summary: String,
}

/// Typed replay failures. Malformed records are rejected, never repaired.
#[derive(Debug)]
pub enum ReplayError {
    SchemaMismatch {
        found: String,
    },
    UnknownStatus {
        name: String,
    },
    SummaryTooLarge {
        limit: usize,
    },
    EmptySummary,
    TooManyRecords {
        limit: usize,
    },
    Decode(serde_json::Error),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaMismatch { found } => write!(
                f,
                "replay record schema '{found}' is unsupported; expected '{REPLAY_SCHEMA}'"
            ),
            Self::UnknownStatus { name } => {
                write!(f, "replay record status '{name}' is not a terminal status")
            }
            Self::SummaryTooLarge { limit } => {
                write!(f, "replay summary exceeds {limit} bytes")
            }
            Self::EmptySummary => write!(f, "replay record summary is empty"),
            Self::TooManyRecords { limit } => {
                write!(f, "more than {limit} replay records in one pass")
            }
            Self::Decode(err) => write!(f, "replay records are malformed: {err}"),
        }
    }
}

impl std::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(err) => Some(err),
            _ => None,
        }
    }
}

/// Persist one batch of scored attempts as replay JSON.
pub fn persist_replay(attempts: &[ScoredAttempt], summaries: &[String]) -> Result<String, ReplayError> {
    if attempts.len() != summaries.len() {
        return Err(ReplayError::Decode(
            <serde_json::Error as serde::de::Error>::custom(
                "attempt and summary counts differ",
            ),
        ));
    }
    let records: Vec<ReplayRecord> = attempts
        .iter()
        .zip(summaries.iter())
        .map(|(attempt, summary)| ReplayRecord {
            schema: REPLAY_SCHEMA.to_string(),
            index: attempt.index,
            status: attempt.status.as_str().to_string(),
            tokens: attempt.tokens,
            summary: summary.clone(),
        })
        .collect();
    serde_json::to_string(&records).map_err(ReplayError::Decode)
}

/// Hydrate persisted records back into scored attempts. Selection over the
/// hydrated list is the post-crash substitute for re-running children.
pub fn hydrate_replay(json: &str) -> Result<Vec<ScoredAttempt>, ReplayError> {
    let records: Vec<ReplayRecord> = serde_json::from_str(json).map_err(ReplayError::Decode)?;
    if records.len() > MAX_REPLAY_RECORDS {
        return Err(ReplayError::TooManyRecords {
            limit: MAX_REPLAY_RECORDS,
        });
    }
    records
        .into_iter()
        .map(|record| {
            if record.schema != REPLAY_SCHEMA {
                return Err(ReplayError::SchemaMismatch {
                    found: record.schema,
                });
            }
            let status = AgentTerminalStatus::ALL
                .iter()
                .copied()
                .find(|status| status.as_str() == record.status)
                .ok_or_else(|| ReplayError::UnknownStatus {
                    name: record.status.clone(),
                })?;
            if record.summary.len() > MAX_REPLAY_SUMMARY_BYTES {
                return Err(ReplayError::SummaryTooLarge {
                    limit: MAX_REPLAY_SUMMARY_BYTES,
                });
            }
            if record.summary.is_empty() {
                return Err(ReplayError::EmptySummary);
            }
            Ok(ScoredAttempt {
                index: record.index,
                status,
                tokens: record.tokens,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(index: usize, status: AgentTerminalStatus, tokens: u64) -> ScoredAttempt {
        ScoredAttempt {
            index,
            status,
            tokens,
        }
    }

    #[test]
    fn selects_the_cheapest_viable_attempt_and_ignores_failures() {
        let attempts = vec![
            attempt(0, AgentTerminalStatus::Failed, 10),
            attempt(1, AgentTerminalStatus::Succeeded, 900),
            attempt(2, AgentTerminalStatus::Cancelled, 5),
            attempt(3, AgentTerminalStatus::Succeeded, 400),
        ];
        let best = AttemptSelector::select_best(&attempts).expect("select");
        assert_eq!(best.index, 3);
        assert_eq!(best.tokens, 400);
    }

    #[test]
    fn ties_resolve_to_the_lowest_index() {
        let attempts = vec![
            attempt(4, AgentTerminalStatus::Succeeded, 100),
            attempt(2, AgentTerminalStatus::Succeeded, 100),
        ];
        assert_eq!(AttemptSelector::select_best(&attempts).expect("select").index, 2);
    }

    #[test]
    fn rejection_reasons_are_typed_and_all_rejected_is_an_error() {
        let failed = attempt(0, AgentTerminalStatus::Failed, 10);
        let cancelled = attempt(1, AgentTerminalStatus::Cancelled, 10);
        assert_eq!(
            AttemptSelector::rejection_of(&failed),
            Some(AttemptRejection::Failed)
        );
        assert_eq!(
            AttemptSelector::rejection_of(&cancelled),
            Some(AttemptRejection::Cancelled)
        );
        let err = AttemptSelector::select_best(&[failed, cancelled]).expect_err("no viable");
        assert_eq!(err, AttemptSelectionError::NoViableAttempt);
    }

    #[test]
    fn selector_rejects_empty_duplicate_and_over_limit_inputs() {
        assert_eq!(
            AttemptSelector::select_best(&[]),
            Err(AttemptSelectionError::NoAttempts)
        );
        let attempts = vec![
            attempt(0, AgentTerminalStatus::Succeeded, 1),
            attempt(0, AgentTerminalStatus::Succeeded, 2),
        ];
        assert_eq!(
            AttemptSelector::select_best(&attempts),
            Err(AttemptSelectionError::DuplicateIndex)
        );
        let too_many: Vec<ScoredAttempt> = (0..=MAX_ATTEMPTS)
            .map(|i| attempt(i, AgentTerminalStatus::Succeeded, 1))
            .collect();
        assert_eq!(
            AttemptSelector::select_best(&too_many),
            Err(AttemptSelectionError::TooManyAttempts {
                limit: MAX_ATTEMPTS
            })
        );
    }

    #[test]
    fn sweep_plans_removal_only_for_abandoned_creations() {
        let records = vec![
            SweepRecord {
                view_id: WorkspaceViewId::new(),
                state: GitWorktreeRecordState::Creating,
            },
            SweepRecord {
                view_id: WorkspaceViewId::new(),
                state: GitWorktreeRecordState::Active,
            },
            SweepRecord {
                view_id: WorkspaceViewId::new(),
                state: GitWorktreeRecordState::CleanupFailed,
            },
        ];
        let plan = SweepPlanner::plan(&records);
        assert_eq!(plan.remove.len(), 1);
        assert_eq!(plan.remove[0], records[0].view_id);
        assert_eq!(plan.retain.len(), 2);
        assert_eq!(plan.retain[0].1, RetainReason::Active);
        assert_eq!(plan.retain[1].1, RetainReason::CleanupFailedNeedsOperator);
    }

    #[test]
    fn sweep_is_idempotent_after_removals_are_applied() {
        let creating = SweepRecord {
            view_id: WorkspaceViewId::new(),
            state: GitWorktreeRecordState::Creating,
        };
        let first = SweepPlanner::plan(&[creating]);
        assert_eq!(first.remove.len(), 1);
        // The host applied the removal: the record is gone from the ledger.
        let after_apply = SweepPlanner::plan(&[]);
        assert!(after_apply.remove.is_empty());
        // Planning twice over unchanged records yields the same plan.
        let again = SweepPlanner::plan(&[creating]);
        assert_eq!(first, again);
    }

    #[test]
    fn replay_persists_and_hydrates_attempts_for_reselection() {
        let attempts = vec![
            attempt(0, AgentTerminalStatus::Failed, 120),
            attempt(1, AgentTerminalStatus::Succeeded, 450),
        ];
        let summaries = vec!["first try failed".to_owned(), "did the work".to_owned()];
        let json = persist_replay(&attempts, &summaries).expect("persist");
        let hydrated = hydrate_replay(&json).expect("hydrate");
        assert_eq!(hydrated, attempts);
        let best = AttemptSelector::select_best(&hydrated).expect("select");
        assert_eq!(best.index, 1);
    }

    #[test]
    fn replay_rejects_wrong_schema_unknown_status_and_oversize() {
        let records = "[{\"schema\":\"rapidlm.subagent.replay.v0\",\"index\":0,\"status\":\"succeeded\",\"tokens\":1,\"summary\":\"x\"}]";
        let err = hydrate_replay(records).expect_err("schema");
        assert!(matches!(err, ReplayError::SchemaMismatch { .. }));
        let bad_status = format!(
            "[{{\"schema\":\"{REPLAY_SCHEMA}\",\"index\":0,\"status\":\"running\",\"tokens\":1,\"summary\":\"x\"}}]"
        );
        let err = hydrate_replay(&bad_status).expect_err("status");
        assert!(matches!(err, ReplayError::UnknownStatus { .. }));
        let big = "y".repeat(MAX_REPLAY_SUMMARY_BYTES + 1);
        let oversize = format!(
            "[{{\"schema\":\"{REPLAY_SCHEMA}\",\"index\":0,\"status\":\"succeeded\",\"tokens\":1,\"summary\":\"{big}\"}}]"
        );
        let err = hydrate_replay(&oversize).expect_err("size");
        assert!(matches!(err, ReplayError::SummaryTooLarge { .. }));
    }

    #[test]
    fn replay_rejects_malformed_json_and_empty_summaries() {
        assert!(matches!(
            hydrate_replay("not json"),
            Err(ReplayError::Decode(_))
        ));
        let empty = format!(
            "[{{\"schema\":\"{REPLAY_SCHEMA}\",\"index\":0,\"status\":\"succeeded\",\"tokens\":1,\"summary\":\"\"}}]"
        );
        assert!(matches!(hydrate_replay(&empty), Err(ReplayError::EmptySummary)));
    }

    #[test]
    fn persist_requires_a_summary_for_every_attempt() {
        let attempts = vec![attempt(0, AgentTerminalStatus::Succeeded, 1)];
        assert!(persist_replay(&attempts, &[]).is_err());
    }
}
