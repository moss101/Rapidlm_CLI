//! Publication: a candidate reaches the user's tree once, with a receipt.
//!
//! ADR 0021 §4 asks that publication be a workspace transaction whose
//! `CommitReceipt` is the publication receipt, with the operation journal
//! recording prepared → applied → reconciled. Two facts about the existing
//! code shape this module:
//!
//! - `workspace::TransactionManager` stages a [`workspace::patch::
//!   SemanticPatch`] into an **in-memory** [`StagingOverlay`] and publishes
//!   it as a parent-visible overlay; its own `begin_transaction` doc says
//!   "Parent checkout is not written". Nothing materializes an overlay onto
//!   disk. Routing `/agents integrate` through it as-is would replace a real
//!   `git apply` with an overlay no one reads — the child's work would stop
//!   reaching the user's files. So the transaction *type* is not on this
//!   path; the transaction *contract* — a frozen parent revision, a patch
//!   hash, a change count, checks, and one at-most-once publication — is.
//! - `event_ledger::journal::OperationJournal` already owns exactly the
//!   vocabulary the ADR names: [`OperationState`] is
//!   `Prepared`/`Executing`/`Committed`/`Failed`/`Uncertain`/`Reconciled`,
//!   with a stable [`EffectFingerprint`] and an
//!   [`IdempotencyClass::AtMostOnce`] that recovery must never replay.
//!
//! So publication is journaled here and applied by git, in this order:
//!
//! 1. **Fingerprint** the effect from the agent, the child's base revision,
//!    the parent's current revision and the patch hash. The same request
//!    fingerprints the same way across processes.
//! 2. **Answer from the journal** when that fingerprint already reached a
//!    terminal committed state — a repeat cannot apply twice.
//! 3. **Prepare** (`Prepared`): the effect has not started; the tree is
//!    untouched.
//! 4. **Execute** (`Executing`): the window in which the tree may change.
//!    A crash here leaves a non-terminal at-most-once record, which recovery
//!    must reconcile rather than replay — [`OperationJournal::
//!    assert_replay_allowed`] enforces that.
//! 5. **Commit** (`Committed`) with the receipt, or **fail** (`Failed`) with
//!    the tree unchanged.
//!
//! A publication that cannot be journaled does not happen: an at-most-once
//! effect we could not record is exactly the one a crash would double-apply.

use std::path::Path;

use event_ledger::journal::{
    CancellationToken, EffectFingerprint, EffectSpec, IdempotencyClass, JournalError, OperationId,
    OperationJournal, OperationState,
};
use event_ledger::ledger::EventLedger;
use protocol::{ArtifactId, ProjectId, SessionId};

/// The action name every workspace publication fingerprints under.
pub const PUBLISH_ACTION: &str = "workspace.publish";

/// `reconcile_ref` for an in-flight effect that recovery found *did* reach
/// the tree. Only this (and `Committed`) means "already published".
pub const RECONCILED_APPLIED: &str = "applied: observed in the tree";

/// `reconcile_ref` for an in-flight effect that recovery found did *not*
/// reach the tree. The work still needs publishing, so a later request for
/// the same effect must be allowed to run rather than answered "already
/// applied" — answering otherwise tells a caller its patch is in the tree
/// when it is not, and a caller that believes that discards the source.
pub const RECONCILED_NOT_APPLIED: &str = "not-applied: absent from the tree";

/// Proof that one patch became parent-visible: which revision it was applied
/// onto and what was applied. The `CommitReceipt` contract of ADR 0021 §4
/// (parent revision + patch hash + change count), carried by a record the
/// operation journal can answer a repeat request from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationReceipt {
    /// The journal operation this publication is recorded under.
    pub operation: OperationId,
    /// The parent revision the patch was applied onto — the baseline the
    /// publication was frozen against.
    pub parent_revision: String,
    /// Content hash of the applied patch.
    pub patch_hash: String,
    /// How many files the patch touched.
    pub change_count: usize,
}

/// What a publication attempt did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Published {
    /// The patch was applied in this call; the receipt is fresh.
    Applied(PublicationReceipt),
    /// This exact effect was already committed; nothing was applied again
    /// and the answer comes from the journal.
    AlreadyApplied(PublicationReceipt),
}

/// Why a publication could not be recorded or completed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationError {
    /// The journal could not be opened or written. Publication fails closed:
    /// an at-most-once effect that cannot be recorded must not be performed.
    Journal(String),
    /// A previous attempt at this exact effect is still in flight
    /// (`Prepared`/`Executing`/`Uncertain`). It must be reconciled before
    /// the same publication is attempted again — replaying it could apply
    /// the patch twice.
    InFlight {
        operation: OperationId,
        state: OperationState,
    },
    /// The effect itself failed; the caller's detail explains it and the
    /// tree is unchanged.
    Failed(String),
    /// The effect was partly applied. The record is `Uncertain`, not
    /// `Failed`: the tree is in an in-between state and a retry must not
    /// simply republish over it.
    PartiallyApplied(String),
}

/// How an effect failed, which decides what the journal records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectFailure {
    /// Nothing was applied; the tree is exactly as it was.
    Untouched(String),
    /// Some of the effect landed. Recording this as a plain failure would
    /// claim the tree is untouched when it is not.
    Partial(String),
}

impl core::fmt::Display for EffectFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Untouched(detail) | Self::Partial(detail) => f.write_str(detail),
        }
    }
}

impl core::fmt::Display for PublicationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Journal(detail) => write!(f, "publication journal: {detail}"),
            Self::InFlight { operation, state } => write!(
                f,
                "a previous publication ({operation}) is {} and must be reconciled first",
                state.as_str()
            ),
            Self::Failed(detail) => write!(f, "{detail}"),
            Self::PartiallyApplied(detail) => write!(
                f,
                "the publication was only partly applied ({detail}); the tree is in an \
                 in-between state and must be reconciled before another attempt"
            ),
        }
    }
}

impl std::error::Error for PublicationError {}

/// The project's operation journal, and the session publications are
/// recorded in.
pub struct PublicationJournal {
    journal: OperationJournal,
    session: SessionId,
}

impl PublicationJournal {
    /// Open the project's journal, creating the publication session if it is
    /// not there yet. The ledger lives where every other project record does.
    pub fn for_project(ledger_path: &Path) -> Result<Self, PublicationError> {
        if let Some(parent) = ledger_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| PublicationError::Journal(err.to_string()))?;
        }
        let ledger = EventLedger::open(ledger_path)
            .map_err(|err| PublicationError::Journal(err.to_string()))?;
        // One stable session for this project's publications: the journal
        // answers a repeat by fingerprint within a session, so the session
        // must be the same across processes for that to work across a
        // restart.
        let session = publication_session(ledger_path);
        let cancel = event_ledger::ledger::CancellationToken::new();
        match ledger.create_session(session, ProjectId::new(), &cancel) {
            Ok(()) | Err(event_ledger::ledger::LedgerError::SessionExists { .. }) => {}
            Err(err) => return Err(PublicationError::Journal(err.to_string())),
        }
        Ok(Self {
            journal: OperationJournal::new(ledger),
            session,
        })
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn journal(&self) -> &OperationJournal {
        &self.journal
    }
}

/// A publication's identity: everything that makes this the *same* effect.
/// The parent revision is part of it, so publishing the same patch onto a
/// moved parent is a different effect and is allowed to run.
pub struct PublicationRequest<'a> {
    pub principal: &'a str,
    /// The revision the patch is applied onto.
    pub parent_revision: &'a str,
    /// The child's base revision, so a rebased child is a different effect.
    pub base_revision: &'a str,
    pub patch: &'a [u8],
    pub change_count: usize,
}

impl PublicationRequest<'_> {
    fn patch_hash(&self) -> String {
        ArtifactId::from_bytes(self.patch).to_string()
    }

    fn spec(&self) -> Result<EffectSpec, PublicationError> {
        EffectSpec::new(
            PUBLISH_ACTION,
            self.principal,
            self.parent_revision,
            format!("base={} patch={}", self.base_revision, self.patch_hash()),
        )
        .map_err(|err: JournalError| PublicationError::Journal(err.to_string()))
    }

    /// This request's effect identity, for scoping a recovery pass to the
    /// one operation the caller can actually evidence.
    pub fn fingerprint(&self) -> Result<EffectFingerprint, PublicationError> {
        Ok(EffectFingerprint::compute(&self.spec()?))
    }

    fn receipt(&self, operation: OperationId) -> PublicationReceipt {
        PublicationReceipt {
            operation,
            parent_revision: self.parent_revision.to_owned(),
            patch_hash: self.patch_hash(),
            change_count: self.change_count,
        }
    }
}

/// Run `apply` as an at-most-once publication.
///
/// `apply` performs the real effect — the write into the user's tree — and
/// is called exactly once, inside the `Executing` window, and only when the
/// journal agrees this effect has not already been committed. It returns
/// `Ok` when the tree changed and `Err(detail)` when it did not.
pub fn publish<F>(
    journal: &PublicationJournal,
    request: &PublicationRequest<'_>,
    apply: F,
) -> Result<Published, PublicationError>
where
    F: FnOnce() -> Result<(), EffectFailure>,
{
    let cancel = CancellationToken::new();
    let spec = request.spec()?;
    let fingerprint = EffectFingerprint::compute(&spec);

    // Answered from the journal: this exact effect already happened, or a
    // previous attempt is still in flight and must be reconciled first.
    if let Some(previous) = journal
        .journal
        .find_by_fingerprint(journal.session, fingerprint, &cancel)
        .map_err(|err| PublicationError::Journal(err.to_string()))?
    {
        match previous.state() {
            OperationState::Committed => {
                return Ok(Published::AlreadyApplied(request.receipt(previous.id())));
            }
            // Reconciled says the effect is settled, not that it landed:
            // recovery records both outcomes in this state. Only a
            // reconciliation that observed the effect in the tree may answer
            // "already applied"; one that observed its absence must let the
            // request run, and an unlabelled one is not evidence of anything.
            OperationState::Reconciled if previous.reconcile_ref() == Some(RECONCILED_APPLIED) => {
                return Ok(Published::AlreadyApplied(request.receipt(previous.id())));
            }
            OperationState::Reconciled => {}
            OperationState::Failed => {}
            state @ (OperationState::Prepared
            | OperationState::Executing
            | OperationState::Uncertain) => {
                return Err(PublicationError::InFlight {
                    operation: previous.id(),
                    state,
                });
            }
        }
    }

    // Prepared: recorded, not started. A crash here leaves nothing applied.
    let record = journal
        .journal
        .prepare(
            journal.session,
            &spec,
            IdempotencyClass::AtMostOnce,
            &cancel,
        )
        .map_err(|err| PublicationError::Journal(err.to_string()))?;
    let operation = record.id();

    // Executing: the window in which the tree may change. A crash between
    // this and the commit leaves a non-terminal at-most-once record, which
    // recovery must reconcile rather than replay.
    journal
        .journal
        .mark_executing(operation, &cancel)
        .map_err(|err| PublicationError::Journal(err.to_string()))?;

    match apply() {
        Ok(()) => {
            journal
                .journal
                .commit(operation, &cancel)
                .map_err(|err| PublicationError::Journal(err.to_string()))?;
            Ok(Published::Applied(request.receipt(operation)))
        }
        // The effect provably did not happen: record it, so the next attempt
        // does not meet an in-flight record it cannot tell from a real crash.
        Err(EffectFailure::Untouched(detail)) => {
            journal.journal.fail(operation, &cancel).map_err(|err| {
                PublicationError::Journal(format!(
                    "the effect failed ({detail}) and the failure could not be recorded: {err}"
                ))
            })?;
            Err(PublicationError::Failed(detail))
        }
        // The effect may be partly done. Recording `Failed` would claim the
        // tree is untouched when it is not, and the next attempt would
        // republish over a half-published tree. `Uncertain` is what this is,
        // and recovery settles it from the tree itself.
        Err(EffectFailure::Partial(detail)) => {
            journal
                .journal
                .mark_uncertain(operation, &cancel)
                .map_err(|err| {
                    PublicationError::Journal(format!(
                        "the effect was partly applied ({detail}) and that could not be recorded: {err}"
                    ))
                })?;
            Err(PublicationError::PartiallyApplied(detail))
        }
    }
}

/// What a recovery pass did to the work a restart inherited.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Operations that had not started (`Prepared`): nothing happened, so
    /// they are closed as failed and a fresh request may run.
    pub abandoned: Vec<OperationId>,
    /// At-most-once operations that were in flight: whether the effect
    /// landed cannot be known from the journal alone, so they are marked
    /// `Uncertain` and then reconciled from the world's own evidence.
    pub reconciled: Vec<OperationId>,
    /// In-flight operations left for a human: the reconciler could not tell
    /// whether the effect landed.
    pub undecided: Vec<OperationId>,
    /// In-flight operations belonging to a different effect, left untouched
    /// because this caller holds no evidence about them.
    pub untouched: Vec<OperationId>,
}

/// What the world says about an in-flight effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Settlement {
    /// The effect is visible; record it as reconciled.
    Applied,
    /// The effect is provably absent; record it as reconciled-not-applied.
    NotApplied,
    /// Cannot tell. The record stays `Uncertain` for a human.
    Unknown,
}

/// Settle the publications a restart inherited, before anything new runs.
///
/// An at-most-once effect that was in flight when the process died is the
/// one case the journal cannot resolve by itself: `Executing` means the
/// patch may or may not have reached the tree. [`ReplayPolicy`] already
/// refuses to replay such a record; recovery's job is to turn it into a
/// terminal one by asking the world — `settle` inspects the actual tree —
/// so the next publication is not blocked forever by
/// [`PublicationError::InFlight`].
///
/// A `Prepared` record is different: the effect had not started, so it is
/// closed as failed and a fresh request is free to run.
pub fn recover<F>(
    journal: &PublicationJournal,
    only: EffectFingerprint,
    mut settle: F,
) -> Result<RecoveryReport, PublicationError>
where
    F: FnMut(&event_ledger::journal::OperationRecord) -> Settlement,
{
    let cancel = CancellationToken::new();
    let pending = journal
        .journal
        .pending(journal.session, &cancel)
        .map_err(|err| PublicationError::Journal(err.to_string()))?;
    let mut report = RecoveryReport::default();
    for record in pending {
        // The publication session is project-wide, so `pending` also returns
        // other agents' in-flight work. A caller can only evidence the effect
        // it holds the patch for; settling someone else's record from this
        // patch would write a terminal answer with no basis — and could
        // reconcile a live operation out from under the process performing
        // it. Anything else is left exactly as it was.
        if record.fingerprint() != only {
            report.untouched.push(record.id());
            continue;
        }
        match record.state() {
            // Nothing happened: close it so a retry is clean. The journal's
            // closed machine has no `Prepared → Failed` edge (its only exit
            // from `Prepared` is `Executing`), so closing a never-started
            // record goes through it. The record ends `Failed`, which is the
            // truth: the operation did not complete. Nothing is executed —
            // the effect closure is not involved in recovery at all.
            OperationState::Prepared => {
                journal
                    .journal
                    .mark_executing(record.id(), &cancel)
                    .map_err(|err| PublicationError::Journal(err.to_string()))?;
                journal
                    .journal
                    .fail(record.id(), &cancel)
                    .map_err(|err| PublicationError::Journal(err.to_string()))?;
                report.abandoned.push(record.id());
            }
            // May or may not have happened. Never replayed — reconciled.
            OperationState::Executing | OperationState::Uncertain => {
                if record.state() == OperationState::Executing {
                    journal
                        .journal
                        .mark_uncertain(record.id(), &cancel)
                        .map_err(|err| PublicationError::Journal(err.to_string()))?;
                }
                match settle(&record) {
                    Settlement::Applied => {
                        journal
                            .journal
                            .reconcile(record.id(), RECONCILED_APPLIED, &cancel)
                            .map_err(|err| PublicationError::Journal(err.to_string()))?;
                        report.reconciled.push(record.id());
                    }
                    Settlement::NotApplied => {
                        journal
                            .journal
                            .reconcile(record.id(), RECONCILED_NOT_APPLIED, &cancel)
                            .map_err(|err| PublicationError::Journal(err.to_string()))?;
                        report.reconciled.push(record.id());
                    }
                    Settlement::Unknown => report.undecided.push(record.id()),
                }
            }
            // `pending` never returns these.
            OperationState::Committed | OperationState::Failed | OperationState::Reconciled => {}
        }
    }
    Ok(report)
}

/// A stable per-project session id for publications, derived from the
/// ledger's own path so the same project resolves the same session across
/// processes — the journal answers a repeat by fingerprint *within* a
/// session, so a fresh random session every run would defeat idempotency.
fn publication_session(ledger_path: &Path) -> SessionId {
    let path = protocol::host_path::canonicalize(ledger_path)
        .unwrap_or_else(|_| ledger_path.to_path_buf());
    let digest = ArtifactId::from_bytes(
        format!("rapidlm.publication.session/v1:{}", path.display()).as_bytes(),
    );
    // `ArtifactId` is a 32-byte digest; take the first 16 as the id's bytes
    // so the mapping is deterministic and collision-resistant enough for a
    // per-project session.
    let bytes = digest.as_digest();
    let hex: String = bytes[..16].iter().map(|b| format!("{b:02x}")).collect();
    // Canonical 8-4-4-4-12 form; `SessionId`'s parser accepts any canonical
    // UUID, so a derived (non-v7) id is a legal, stable identifier here.
    let canonical = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    canonical
        .parse()
        .expect("a 32-hex-digit digest always forms a canonical UUID")
}
