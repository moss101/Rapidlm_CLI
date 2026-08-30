//! Foreground→daemon execution ownership transfer (P7-006).
//!
//! Implements the durable handoff protocol that transfers execution-generation
//! ownership from a foreground CLI process to a background daemon. Enforces the
//! single-writer invariant across processes via file-based persistence.
//!
//! Flow:
//! 1. Foreground owns generation N.
//! 2. `detach()` writes a [`HandoffBundle`] to disk and relinquishes authority.
//! 3. Daemon calls `accept()` which validates the bundle and bumps generation.
//! 4. Duplicate-writer rejection: only one active writer per session.
//! 5. Reconnect observes the same session/execution state.
//!
//! This reuses `protocol::HandoffId` for stable identity. It does NOT create a
//! second session or handoff protocol — it is the canonical implementation of
//! the existing V3 ownership-transfer contract.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use protocol::{HandoffId, SessionId};
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 bytes accepted in an owner-id string.
pub const MAX_OWNER_BYTES: usize = 128;

/// Maximum UTF-8 bytes accepted in a lease/credential reference string.
pub const MAX_REF_BYTES: usize = 256;

/// Typed execution-generation writer lease with a hard expiry. Presence of
/// a valid lease is the only write-authority signal for a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionLease {
    pub session_id: SessionId,
    pub generation: u64,
    pub owner: String,
    /// Unix-millis after which this lease no longer confers authority.
    pub expires_at_unix_ms: u64,
}

impl SessionExecutionLease {
    /// A lease past its expiry confers no write authority.
    pub fn expired(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }
}

/// Reference to one freshly reissued target-side artifact. Bounded data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedRef {
    /// Kind of artifact: e.g. "lease", "credential", "observation".
    pub kind: String,
    /// Opaque identifier of the reissued artifact (id or fingerprint).
    pub id: String,
}

impl IssuedRef {
    pub fn new(kind: &str, id: &str) -> Result<Self, HandoffError> {
        let valid =
            |s: &str| !s.is_empty() && s.len() <= MAX_REF_BYTES && !s.chars().any(char::is_control);
        if !valid(kind) || !valid(id) {
            return Err(HandoffError::InvalidOwner);
        }
        Ok(Self {
            kind: kind.to_owned(),
            id: id.to_owned(),
        })
    }
}

/// Fresh target-side issuance recorded between generation commit and remote
/// resume. Leases and observations are mandatory (the target must never
/// inherit the source's view); credentials are optional when the target uses
/// none. Nothing here carries plaintext secret material.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshIssuance {
    pub generation: u64,
    pub refs: Vec<IssuedRef>,
}

impl FreshIssuance {
    /// Validate issuance for an accepted generation: at least one reissued
    /// lease and one fresh observation must be present.
    pub fn for_generation(generation: u64, refs: Vec<IssuedRef>) -> Result<Self, HandoffError> {
        let has_lease = refs.iter().any(|r| r.kind == "lease");
        let has_observation = refs.iter().any(|r| r.kind == "observation");
        if !has_lease || !has_observation {
            return Err(HandoffError::IncompleteIssuance);
        }
        Ok(Self {
            generation,
            refs,
        })
    }
}

/// Durable handoff bundle written by the detaching owner and consumed by the
/// accepting daemon. Serialized as JSON for cross-process visibility.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffBundle {
    /// Stable handoff identity.
    pub id: HandoffId,
    /// The session whose execution is being handed off.
    pub session_id: SessionId,
    /// The generation being relinquished by the detaching owner.
    pub relinquished_generation: u64,
    /// The generation the accepting daemon will own (N+1).
    pub accepted_generation: u64,
    /// Who detached.
    pub previous_owner: String,
    /// Who is expected to accept.
    pub new_owner: String,
    /// Unix-millis when the detach was initiated.
    pub detached_at_unix_ms: u64,
}

/// Current execution ownership state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOwnership {
    pub session_id: SessionId,
    pub generation: u64,
    pub owner: String,
}

/// Typed handoff failure. Display never echoes session/goal text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandoffError {
    AlreadyOwned { by: String },
    NotOwned,
    GenerationMismatch { expected: u64, found: u64 },
    InvalidOwner,
    LeaseExpired,
    IncompleteIssuance,
    InvalidPhase,
    Io,
    Serialization,
}

impl fmt::Display for HandoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyOwned { .. } => "session already owned by another writer",
            Self::NotOwned => "no active execution ownership to detach",
            Self::GenerationMismatch { .. } => "generation mismatch during handoff",
            Self::InvalidOwner => "owner id is invalid",
            Self::LeaseExpired => "execution lease has expired",
            Self::IncompleteIssuance => {
                "fresh issuance requires at least one lease and one observation"
            }
            Self::InvalidPhase => "handoff transition is not valid in the current phase",
            Self::Io => "handoff persistence I/O error",
            Self::Serialization => "handoff serialization failed",
        })
    }
}

impl Error for HandoffError {}

/// Validate an owner string.
fn valid_owner(owner: &str) -> bool {
    !owner.is_empty() && owner.len() <= MAX_OWNER_BYTES && !owner.chars().any(char::is_control)
}

/// Canonical ownership-transfer ledger persisted to disk.
pub struct OwnershipLedger {
    path: PathBuf,
}

impl OwnershipLedger {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load current ownership state, if any.
    pub fn load(&self) -> Result<Option<ExecutionOwnership>, HandoffError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let ownership: ExecutionOwnership =
                    serde_json::from_slice(&bytes).map_err(|_| HandoffError::Serialization)?;
                Ok(Some(ownership))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(HandoffError::Io),
        }
    }

    /// Persist ownership state atomically.
    pub fn save(&self, ownership: &ExecutionOwnership) -> Result<(), HandoffError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| HandoffError::Io)?;
        }
        let json =
            serde_json::to_string_pretty(ownership).map_err(|_| HandoffError::Serialization)?;
        // Atomic write: write temp then rename.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|_| HandoffError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(|_| HandoffError::Io)?;
        Ok(())
    }

    /// Clear ownership (daemon stopped / session ended).
    pub fn clear(&self) -> Result<(), HandoffError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(HandoffError::Io),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Acquire execution ownership for a session at a specific generation.
/// Rejects duplicate writers when another owner already holds the lease.
pub fn acquire(
    ledger: &OwnershipLedger,
    session_id: SessionId,
    owner: &str,
    generation: u64,
) -> Result<ExecutionOwnership, HandoffError> {
    if !valid_owner(owner) {
        return Err(HandoffError::InvalidOwner);
    }
    if let Some(existing) = ledger.load()? {
        return Err(HandoffError::AlreadyOwned {
            by: existing.owner.clone(),
        });
    }
    let ownership = ExecutionOwnership {
        session_id,
        generation,
        owner: owner.to_owned(),
    };
    ledger.save(&ownership)?;
    Ok(ownership)
}

/// Detach: relinquish ownership and create a durable HandoffBundle.
pub fn detach(
    ledger: &OwnershipLedger,
    session_id: SessionId,
    previous_owner: &str,
    new_owner: &str,
    now_unix_ms: u64,
) -> Result<HandoffBundle, HandoffError> {
    if !valid_owner(previous_owner) || !valid_owner(new_owner) {
        return Err(HandoffError::InvalidOwner);
    }
    let ownership = ledger.load()?.ok_or(HandoffError::NotOwned)?;
    if ownership.owner != previous_owner {
        return Err(HandoffError::AlreadyOwned {
            by: ownership.owner.clone(),
        });
    }
    if ownership.session_id != session_id {
        return Err(HandoffError::InvalidOwner);
    }
    let bundle = HandoffBundle {
        id: HandoffId::new(),
        session_id,
        relinquished_generation: ownership.generation,
        accepted_generation: ownership.generation + 1,
        previous_owner: previous_owner.to_owned(),
        new_owner: new_owner.to_owned(),
        detached_at_unix_ms: now_unix_ms,
    };
    // Relinquish: clear the ownership record so the daemon can accept.
    ledger.clear()?;
    Ok(bundle)
}

/// Accept: daemon takes ownership at generation N+1 from the handoff bundle.
pub fn accept(
    ledger: &OwnershipLedger,
    bundle: &HandoffBundle,
    new_owner: &str,
) -> Result<ExecutionOwnership, HandoffError> {
    if !valid_owner(new_owner) {
        return Err(HandoffError::InvalidOwner);
    }
    if let Some(existing) = ledger.load()? {
        return Err(HandoffError::AlreadyOwned {
            by: existing.owner.clone(),
        });
    }
    if new_owner != bundle.new_owner {
        return Err(HandoffError::InvalidOwner);
    }
    let ownership = ExecutionOwnership {
        session_id: bundle.session_id,
        generation: bundle.accepted_generation,
        owner: new_owner.to_owned(),
    };
    ledger.save(&ownership)?;
    Ok(ownership)
}

/// Reject a duplicate writer attempting to acquire while owned.
pub fn reject_duplicate_writer(
    ledger: &OwnershipLedger,
    attempted_by: &str,
) -> Result<(), HandoffError> {
    let Some(existing) = ledger.load()? else {
        return Ok(()); // no owner, no conflict
    };
    Err(HandoffError::AlreadyOwned {
        by: format!("{} ({attempted_by} rejected)", existing.owner),
    })
}

/// Acquire a bounded [`SessionExecutionLease`] at a specific generation.
pub fn acquire_lease(
    ledger: &OwnershipLedger,
    session_id: SessionId,
    owner: &str,
    generation: u64,
    now_unix_ms: u64,
    ttl_ms: u64,
) -> Result<SessionExecutionLease, HandoffError> {
    acquire(ledger, session_id, owner, generation)?;
    Ok(SessionExecutionLease {
        session_id,
        generation,
        owner: owner.to_owned(),
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
    })
}

/// Accept a handoff bundle under an expiry check: an expired source-side
/// view cannot be promoted; the accepting side mints its own fresh lease.
pub fn accept_lease(
    ledger: &OwnershipLedger,
    bundle: &HandoffBundle,
    new_owner: &str,
    now_unix_ms: u64,
    ttl_ms: u64,
) -> Result<SessionExecutionLease, HandoffError> {
    if now_unix_ms >= bundle.detached_at_unix_ms.saturating_add(MAX_BUNDLE_AGE_MS) {
        return Err(HandoffError::LeaseExpired);
    }
    let ownership = accept(ledger, bundle, new_owner)?;
    Ok(SessionExecutionLease {
        session_id: ownership.session_id,
        generation: ownership.generation,
        owner: ownership.owner,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
    })
}

/// A stale bundle older than this cannot be accepted; the target must not
/// resurrect long-dead sessions.
pub const MAX_BUNDLE_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Local↔remote handoff phases in architecture order:
/// quiesce -> durable bundle -> target restore/verify -> commit ->
/// fresh leases/observations -> resume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffPhase {
    /// Source owns the only valid write-capable generation.
    LocalAuthoritative,
    /// Source stopped mutating; bundle not yet durable.
    Quiesced,
    /// Durable bundle written and source relinquished authority.
    Bundled,
    /// Target restored from the bundle and verified it.
    TargetRestored,
    /// Generation transfer committed; target holds generation N+1.
    GenerationCommitted,
    /// Fresh leases/observations issued; resume is allowed next.
    FreshIssued,
    /// Remote (target) execution resumed.
    RemoteResumed,
    /// Failure/partition before commit: session parked, no target writes.
    Parked,
}

/// Explicit local↔remote handoff state machine. Every transition is
/// validated in order; failure before [`HandoffPhase::GenerationCommitted`]
/// parks the attempt leaving the source authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffStateMachine {
    phase: HandoffPhase,
}

impl HandoffStateMachine {
    pub fn new() -> Self {
        Self {
            phase: HandoffPhase::LocalAuthoritative,
        }
    }

    pub fn phase(&self) -> HandoffPhase {
        self.phase
    }

    fn expect(&self, expected: HandoffPhase) -> Result<(), HandoffError> {
        if self.phase != expected {
            return Err(HandoffError::InvalidPhase);
        }
        Ok(())
    }

    /// Source stops mutating.
    pub fn quiesce(&mut self) -> Result<(), HandoffError> {
        self.expect(HandoffPhase::LocalAuthoritative)?;
        self.phase = HandoffPhase::Quiesced;
        Ok(())
    }

    /// Durable bundle written and handed to the target.
    pub fn bundle(&mut self) -> Result<(), HandoffError> {
        self.expect(HandoffPhase::Quiesced)?;
        self.phase = HandoffPhase::Bundled;
        Ok(())
    }

    /// Target restores and verifies the bundle contents.
    pub fn restore_verified(&mut self) -> Result<(), HandoffError> {
        self.expect(HandoffPhase::Bundled)?;
        self.phase = HandoffPhase::TargetRestored;
        Ok(())
    }

    /// Commit the generation transfer (N -> N+1). Only after this point may
    /// the target hold write authority.
    pub fn commit_generation(&mut self) -> Result<(), HandoffError> {
        self.expect(HandoffPhase::TargetRestored)?;
        self.phase = HandoffPhase::GenerationCommitted;
        Ok(())
    }

    /// Record fresh issuance. Requires at least one reissued lease and one
    /// fresh observation so the target never inherits source-side state.
    pub fn issue_fresh(&mut self, issuance: &FreshIssuance) -> Result<(), HandoffError> {
        if self.phase != HandoffPhase::GenerationCommitted {
            return Err(HandoffError::InvalidPhase);
        }
        if issuance.generation == 0 {
            return Err(HandoffError::InvalidOwner);
        }
        FreshIssuance::for_generation(issuance.generation, issuance.refs.clone())?;
        self.phase = HandoffPhase::FreshIssued;
        Ok(())
    }

    /// Resume remote execution. Only valid once fresh issuance completed.
    pub fn resume_remote(&mut self) -> Result<(), HandoffError> {
        self.expect(HandoffPhase::FreshIssued)?;
        self.phase = HandoffPhase::RemoteResumed;
        Ok(())
    }

    /// Partition/failure at any pre-commit phase parks the session with no
    /// target write authority. Post-commit failures cannot park (the target
    /// is already authoritative); they must complete via issuance/resume.
    pub fn park(&mut self) -> Result<HandoffPhase, HandoffError> {
        match self.phase {
            HandoffPhase::LocalAuthoritative
            | HandoffPhase::Quiesced
            | HandoffPhase::Bundled
            | HandoffPhase::TargetRestored => {
                self.phase = HandoffPhase::Parked;
                Ok(self.phase)
            }
            _ => Err(HandoffError::InvalidPhase),
        }
    }
}

impl Default for HandoffStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn session() -> SessionId {
        SessionId::from_str("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("session")
    }

    fn other_session() -> SessionId {
        SessionId::from_str("018f3c8a-7e2b-7a10-8c4d-fedcba987654").expect("session")
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rapidlm-handoff-{name}-{}", std::process::id()))
    }

    #[test]
    fn acquire_then_duplicate_writer_rejected() {
        let path = scratch("acquire");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        let ownership = super::acquire(&ledger, session(), "foreground", 1).expect("acquire");
        assert_eq!(ownership.generation, 1);
        assert_eq!(ownership.owner, "foreground");
        // Duplicate writer rejected.
        assert!(super::reject_duplicate_writer(&ledger, "daemon").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detach_relinquishes_then_daemon_accepts_next_generation() {
        let path = scratch("detach");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        super::acquire(&ledger, session(), "foreground", 1).expect("acquire");
        let bundle = super::detach(&ledger, session(), "foreground", "daemon", 1000).expect("detach");
        assert_eq!(bundle.relinquished_generation, 1);
        assert_eq!(bundle.accepted_generation, 2);
        // After detach, no owner exists (foreground relinquished).
        assert!(ledger.load().expect("load").is_none());
        // Daemon accepts generation 2.
        let ownership = super::accept(&ledger, &bundle, "daemon").expect("accept");
        assert_eq!(ownership.generation, 2);
        assert_eq!(ownership.owner, "daemon");
        // Duplicate accept rejected.
        assert!(matches!(
            super::accept(&ledger, &bundle, "other-daemon"),
            Err(HandoffError::AlreadyOwned { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn accept_rejects_a_caller_that_is_not_the_bundles_expected_new_owner() {
        let path = scratch("accept-wrong-owner");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        super::acquire(&ledger, session(), "foreground", 1).expect("acquire");
        let bundle = super::detach(&ledger, session(), "foreground", "daemon", 1000).expect("detach");
        // Ledger is empty right after detach; a third party must not be able
        // to accept under a different owner string than the bundle names.
        assert!(ledger.load().expect("load").is_none());
        assert!(matches!(
            super::accept(&ledger, &bundle, "attacker"),
            Err(HandoffError::InvalidOwner)
        ));
        // The rejected attempt must not have taken ownership.
        assert!(ledger.load().expect("load").is_none());
        // The legitimate owner can still accept afterward.
        let ownership = super::accept(&ledger, &bundle, "daemon").expect("accept");
        assert_eq!(ownership.owner, "daemon");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detach_rejects_a_session_id_that_does_not_match_the_owned_session() {
        let path = scratch("detach-wrong-session");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        super::acquire(&ledger, session(), "foreground", 1).expect("acquire");
        assert!(matches!(
            super::detach(&ledger, other_session(), "foreground", "daemon", 1000),
            Err(HandoffError::InvalidOwner)
        ));
        // Ownership must be untouched by the rejected attempt.
        let still_owned = ledger.load().expect("load").expect("still owned");
        assert_eq!(still_owned.session_id, session());
        assert_eq!(still_owned.owner, "foreground");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detach_without_ownership_is_rejected() {
        let path = scratch("no-owner");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        assert!(
            super::detach(&ledger, session(), "fg", "dm", 0).is_err(),
            "cannot detach without ownership"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lease_transfer_validates_expiry_and_bumps_generation() {
        let path = scratch("lease");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        let lease =
            super::acquire_lease(&ledger, session(), "foreground", 7, 1_000, 60_000).expect("acquire");
        assert_eq!(lease.generation, 7);
        assert!(!lease.expired(1_000));
        assert!(lease.expired(61_000));

        let bundle = super::detach(&ledger, session(), "foreground", "daemon", 2_000).expect("detach");
        // Fresh bundle accepts and mints a target lease at N+1.
        let accepted = super::accept_lease(&ledger, &bundle, "daemon", 3_000, 60_000).expect("accept");
        assert_eq!(accepted.generation, 8);
        assert_eq!(accepted.owner, "daemon");
        assert!(!accepted.expired(3_000));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_bundle_cannot_be_accepted() {
        let path = scratch("stale-bundle");
        let _ = std::fs::remove_file(&path);
        let ledger = OwnershipLedger::new(path.clone());
        super::acquire(&ledger, session(), "foreground", 1).expect("acquire");
        let bundle = super::detach(&ledger, session(), "foreground", "daemon", 1).expect("detach");
        // Accept attempt far beyond MAX_BUNDLE_AGE_MS must fail closed.
        let too_late = 1 + super::MAX_BUNDLE_AGE_MS + 1;
        assert!(matches!(
            super::accept_lease(&ledger, &bundle, "daemon", too_late, 60_000),
            Err(HandoffError::LeaseExpired)
        ));
        let _ = std::fs::remove_file(&path);
    }

    fn refs() -> Vec<super::IssuedRef> {
        vec![
            super::IssuedRef::new("lease", "lease-9").expect("ref"),
            super::IssuedRef::new("observation", "obs-1").expect("ref"),
            super::IssuedRef::new("credential", "sha256:abc").expect("ref"),
        ]
    }

    #[test]
    fn state_machine_happy_path_follows_architecture_sequence() {
        let mut sm = super::HandoffStateMachine::new();
        assert_eq!(sm.phase(), super::HandoffPhase::LocalAuthoritative);
        sm.quiesce().expect("quiesce");
        sm.bundle().expect("bundle");
        sm.restore_verified().expect("restore");
        sm.commit_generation().expect("commit");
        sm.issue_fresh(&super::FreshIssuance::for_generation(2, refs()).expect("issuance"))
            .expect("issue fresh");
        sm.resume_remote().expect("resume");
        assert_eq!(sm.phase(), super::HandoffPhase::RemoteResumed);
    }

    #[test]
    fn out_of_order_transitions_fail_closed() {
        let mut sm = super::HandoffStateMachine::new();
        // Cannot commit before quiesce/bundle/restore.
        assert!(matches!(
            sm.commit_generation(),
            Err(super::HandoffError::InvalidPhase)
        ));
        // Cannot resume from the start.
        assert!(matches!(
            sm.resume_remote(),
            Err(super::HandoffError::InvalidPhase)
        ));
        sm.quiesce().expect("quiesce");
        // Cannot skip straight to restore before bundling.
        assert!(matches!(
            sm.restore_verified(),
            Err(super::HandoffError::InvalidPhase)
        ));
    }

    #[test]
    fn target_cannot_write_until_commit_and_resume_needs_fresh_issuance() {
        let mut sm = super::HandoffStateMachine::new();
        sm.quiesce().expect("quiesce");
        sm.bundle().expect("bundle");
        sm.restore_verified().expect("restore");
        // Pre-commit failure parks the session with no target writes.
        let mut parked = sm.clone();
        assert_eq!(
            parked.park().expect("park"),
            super::HandoffPhase::Parked
        );
        assert!(matches!(
            parked.commit_generation(),
            Err(super::HandoffError::InvalidPhase)
        ));
        // Committed but without issuance, remote resume is rejected.
        sm.commit_generation().expect("commit");
        // Issuance without a fresh observation is incomplete.
        let lease_only = super::FreshIssuance {
            generation: 2,
            refs: vec![super::IssuedRef::new("lease", "l").expect("r")],
        };
        assert!(matches!(
            sm.issue_fresh(&lease_only),
            Err(super::HandoffError::IncompleteIssuance)
        ));
        assert!(matches!(
            sm.resume_remote(),
            Err(super::HandoffError::InvalidPhase)
        ));
        sm.issue_fresh(&super::FreshIssuance::for_generation(2, refs()).expect("ok"))
            .expect("issue");
        sm.resume_remote().expect("resume");
    }
}
