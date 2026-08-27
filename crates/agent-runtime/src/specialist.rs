//! Persistent read-only specialist control-plane (agent-harness §3, P5-016/P5-017).
//!
//! A [`PersistentSpecialist`] is a long-lived, observation-oriented Context
//! Curator / Explorer that is read-only by default. It is NOT a worker kept
//! alive longer: it has a bounded mailbox, bounded retained summary, repository/
//! context generation tracking, cancellation, and a typed message interface. It
//! never holds a workspace write lease, never accumulates a parent transcript,
//! and cannot mark a parent goal complete. This is control-plane metadata; the
//! scheduler still performs actual execution and admission.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::AgentId;

use crate::agent::model::AgentRole;
use crate::role_profile::{RoleProfile, RoleRegistry};

/// Default bounded mailbox capacity for one specialist.
pub const DEFAULT_SPECIALIST_MAILBOX: usize = 64;

/// Default bounded retained-summary bytes for one specialist.
pub const DEFAULT_SPECIALIST_SUMMARY_BYTES: usize = 8 * 1024;

/// Typed message passed to/from a specialist. Never carries whole transcripts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpecialistMessage {
    /// A bounded observation probe (path/locator + generation) to read.
    Observe { locator: String, generation: u64 },
    /// A bounded, typed result delivered back to the orchestrator.
    Result { summary: String, generation: u64 },
    /// Invalidate stale observations after a workspace generation change.
    Invalidate { generation: u64 },
}

/// Bounds for one specialist. All capacities are finite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpecialistConfig {
    mailbox_limit: usize,
    summary_limit: usize,
}

impl SpecialistConfig {
    pub const fn new(mailbox_limit: usize, summary_limit: usize) -> Result<Self, SpecialistError> {
        if mailbox_limit == 0 || summary_limit == 0 {
            return Err(SpecialistError::InvalidConfig);
        }
        Ok(Self {
            mailbox_limit,
            summary_limit,
        })
    }

    pub const fn mailbox_limit(self) -> usize {
        self.mailbox_limit
    }

    pub const fn summary_limit(self) -> usize {
        self.summary_limit
    }
}

impl Default for SpecialistConfig {
    fn default() -> Self {
        Self {
            mailbox_limit: DEFAULT_SPECIALIST_MAILBOX,
            summary_limit: DEFAULT_SPECIALIST_SUMMARY_BYTES,
        }
    }
}

/// One long-lived read-only specialist.
pub struct PersistentSpecialist {
    id: AgentId,
    role: AgentRole,
    profile: RoleProfile,
    config: SpecialistConfig,
    mailbox: VecDeque<SpecialistMessage>,
    retained_summary: String,
    observed_generation: u64,
    cancelled: AtomicBool,
}

/// Typed specialist failure. Display never includes message or path text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecialistError {
    Cancelled,
    InvalidConfig,
    NotReadOnly,
    MailboxFull,
    SummaryExceeded,
    BoundExceeded,
}

impl PersistentSpecialist {
    /// Build a specialist bound to a read-only role. A non-read-only role is
    /// rejected at the boundary so a specialist can never mutate the governed
    /// workspace.
    pub fn new(
        id: AgentId,
        role: AgentRole,
        config: SpecialistConfig,
    ) -> Result<Self, SpecialistError> {
        let profile = RoleRegistry::profile(role);
        if !profile.read_only() {
            return Err(SpecialistError::NotReadOnly);
        }
        Ok(Self {
            id,
            role,
            profile,
            config,
            mailbox: VecDeque::new(),
            retained_summary: String::new(),
            observed_generation: 0,
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn role(&self) -> AgentRole {
        self.role
    }

    pub fn profile(&self) -> RoleProfile {
        self.profile
    }

    /// Specialists are never writable against the governed workspace.
    pub fn can_mutate_workspace(&self) -> bool {
        false
    }

    /// Specialists never hold an ambient parent capability lease.
    pub fn inherits_parent_lease(&self) -> bool {
        false
    }

    /// Specialists can never complete an autonomous top-level goal.
    pub fn can_complete_goal(&self) -> bool {
        false
    }

    /// Track the observed repository/context generation.
    ///
    /// A generation change invalidates observations from older generations so a
    /// specialist never serves stale repository state.
    pub fn mark_observed(&mut self, generation: u64) -> Result<(), SpecialistError> {
        self.check_cancel()?;
        if generation != self.observed_generation {
            self.mailbox
                .retain(message_generation_is_current(generation));
        }
        self.observed_generation = generation;
        Ok(())
    }

    pub fn observed_generation(&self) -> u64 {
        self.observed_generation
    }

    /// Deliver a typed message into a bounded mailbox.
    pub fn deliver(&mut self, message: SpecialistMessage) -> Result<(), SpecialistError> {
        self.check_cancel()?;
        if self.mailbox.len() >= self.config.mailbox_limit {
            return Err(SpecialistError::MailboxFull);
        }
        self.mailbox.push_back(message);
        Ok(())
    }

    pub fn mailbox(&mut self) -> &[SpecialistMessage] {
        self.mailbox.make_contiguous()
    }

    pub fn pop_mail(&mut self) -> Option<SpecialistMessage> {
        self.mailbox.pop_front()
    }

    /// Retain a bounded summary; the full parent transcript is never stored.
    pub fn retain_summary(&mut self, text: &str) -> Result<(), SpecialistError> {
        self.check_cancel()?;
        if text.len() > self.config.summary_limit {
            return Err(SpecialistError::SummaryExceeded);
        }
        // Bounded memory: only the latest summary is retained.
        self.retained_summary = text.to_owned();
        Ok(())
    }

    pub fn summary(&self) -> &str {
        &self.retained_summary
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn check_cancel(&self) -> Result<(), SpecialistError> {
        if self.is_cancelled() {
            Err(SpecialistError::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn message_generation_is_current(current: u64) -> impl Fn(&SpecialistMessage) -> bool {
    move |message| match message {
        SpecialistMessage::Observe { generation, .. }
        | SpecialistMessage::Result { generation, .. }
        | SpecialistMessage::Invalidate { generation } => *generation >= current,
    }
}

/// Bounded pool-admission for persistent read-only specialists (P5-016/P5-017).
///
/// This is the admission gate in front of the scheduler/spawn path: registering a
/// specialist enforces read-only authority and a finite pool bound, and routes
/// typed Observe/Result messages plus generation invalidation. It is not a second
/// scheduler; admission still owns the actual execution read path.
pub struct SpecialistPool {
    limit: usize,
    specialists: BTreeMap<AgentId, PersistentSpecialist>,
}

impl SpecialistPool {
    pub const fn with_limit(limit: usize) -> Result<Self, SpecialistError> {
        if limit == 0 {
            return Err(SpecialistError::InvalidConfig);
        }
        Ok(Self {
            limit,
            specialists: BTreeMap::new(),
        })
    }

    /// Admit a specialist. A non-read-only specialist is refused at the pool
    /// boundary (authority cannot be widened by admission).
    pub fn register(
        &mut self,
        specialist: PersistentSpecialist,
    ) -> Result<AgentId, SpecialistError> {
        if specialist.can_mutate_workspace() {
            return Err(SpecialistError::NotReadOnly);
        }
        if self.specialists.len() >= self.limit {
            return Err(SpecialistError::BoundExceeded);
        }
        let id = specialist.id();
        self.specialists.insert(id, specialist);
        Ok(id)
    }

    pub fn get(&self, id: AgentId) -> Option<&PersistentSpecialist> {
        self.specialists.get(&id)
    }

    pub fn get_mut(&mut self, id: AgentId) -> Option<&mut PersistentSpecialist> {
        self.specialists.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.specialists.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specialists.is_empty()
    }

    pub fn cancel_all(&self) {
        for specialist in self.specialists.values() {
            specialist.cancel();
        }
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn specialist(role: AgentRole) -> PersistentSpecialist {
        PersistentSpecialist::new(
            AgentId::new(),
            role,
            SpecialistConfig::new(2, 32).expect("config"),
        )
        .expect("specialist")
    }

    #[test]
    fn pool_registration_is_bounded_and_read_only() {
        let mut pool = SpecialistPool::with_limit(2).expect("pool");
        let a = pool.register(specialist(AgentRole::Explorer)).expect("a");
        let b = pool
            .register(specialist(AgentRole::ContextCurator))
            .expect("b");
        assert_eq!(pool.len(), 2);
        let third = specialist(AgentRole::Verifier);
        assert_eq!(pool.register(third), Err(SpecialistError::BoundExceeded));
        // Every admitted specialist is read-only and cannot mutate the workspace.
        for id in [a, b] {
            let s = pool.get(id).expect("specialist");
            assert!(!s.can_mutate_workspace());
            assert!(!s.inherits_parent_lease());
            assert!(!s.can_complete_goal());
            assert!(s.profile().read_only());
        }
    }

    #[test]
    fn pool_routes_typed_messages_and_generation_via_mut_access() {
        let mut pool = SpecialistPool::with_limit(4).expect("pool");
        let id = pool
            .register(specialist(AgentRole::Explorer))
            .expect("register");
        pool.get_mut(id)
            .expect("get")
            .mark_observed(3)
            .expect("gen3");
        pool.get_mut(id)
            .expect("get")
            .deliver(SpecialistMessage::Observe {
                locator: "src/lib.rs".into(),
                generation: 3,
            })
            .expect("deliver");
        let s = pool.get(id).expect("specialist");
        assert_eq!(s.observed_generation(), 3);
        assert_eq!(pool.get_mut(id).expect("get_mut").mailbox().len(), 1);
    }

    #[test]
    fn cancel_all_terminates_every_admitted_specialist() {
        let mut pool = SpecialistPool::with_limit(4).expect("pool");
        let a = pool.register(specialist(AgentRole::Explorer)).expect("a");
        let b = pool
            .register(specialist(AgentRole::ContextCurator))
            .expect("b");
        pool.cancel_all();
        assert!(pool.get(a).expect("a").is_cancelled());
        assert!(pool.get(b).expect("b").is_cancelled());
    }
}

impl fmt::Display for SpecialistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "specialist cancelled",
            Self::InvalidConfig => "specialist config is invalid",
            Self::NotReadOnly => "specialist role must be read-only",
            Self::MailboxFull => "specialist mailbox is full",
            Self::SummaryExceeded => "specialist retained summary exceeds the limit",
            Self::BoundExceeded => "specialist resource bound exceeded",
        })
    }
}

impl Error for SpecialistError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn specialist(role: AgentRole) -> PersistentSpecialist {
        PersistentSpecialist::new(
            AgentId::new(),
            role,
            SpecialistConfig::new(3, 64).expect("config"),
        )
        .expect("specialist")
    }

    #[test]
    fn read_only_only_and_never_writes_or_inherits_or_completes() {
        // A non-read-only role cannot become a specialist.
        assert!(matches!(
            PersistentSpecialist::new(
                AgentId::new(),
                AgentRole::Coder,
                SpecialistConfig::default()
            ),
            Err(SpecialistError::NotReadOnly)
        ));
        for role in [
            AgentRole::Explorer,
            AgentRole::ContextCurator,
            AgentRole::Verifier,
        ] {
            let s = specialist(role);
            assert!(!s.can_mutate_workspace());
            assert!(!s.inherits_parent_lease());
            assert!(!s.can_complete_goal());
            assert!(s.profile().read_only());
        }
    }

    #[test]
    fn mailbox_is_bounded_and_typed() {
        let mut s = specialist(AgentRole::Explorer);
        let msg = SpecialistMessage::Observe {
            locator: "src/lib.rs".to_owned(),
            generation: 1,
        };
        s.deliver(msg.clone()).expect("1");
        s.deliver(SpecialistMessage::Result {
            summary: "ok".to_owned(),
            generation: 1,
        })
        .expect("2");
        s.deliver(msg).expect("3");
        assert_eq!(
            s.deliver(SpecialistMessage::Invalidate { generation: 2 }),
            Err(SpecialistError::MailboxFull)
        );
        assert_eq!(s.mailbox().len(), 3);
        assert!(matches!(
            s.pop_mail(),
            Some(SpecialistMessage::Observe { .. })
        ));
    }

    #[test]
    fn retained_summary_is_bounded_and_parent_transcript_never_accumulates() {
        let mut s = specialist(AgentRole::ContextCurator);
        s.retain_summary("first").expect("a");
        s.retain_summary("second").expect("b");
        assert_eq!(
            s.summary(),
            "second",
            "only the latest bounded summary is retained"
        );
        assert!(s.retain_summary(&"x".repeat(65)).is_err());
    }

    #[test]
    fn generation_changes_invalidate_stale_observations() {
        let mut s = specialist(AgentRole::Explorer);
        s.mark_observed(5).expect("gen5");
        s.deliver(SpecialistMessage::Observe {
            locator: "a".into(),
            generation: 5,
        })
        .expect("observe a");
        s.deliver(SpecialistMessage::Observe {
            locator: "b".into(),
            generation: 7,
        })
        .expect("observe b");
        // Advancing to generation 7 invalidates the stale generation-5 observation.
        s.mark_observed(7).expect("gen7");
        let observations: Vec<_> = s
            .mailbox()
            .iter()
            .filter(|m| matches!(m, SpecialistMessage::Observe { .. }))
            .collect();
        assert_eq!(observations.len(), 1);
        assert!(
            matches!(
                observations[0],
                SpecialistMessage::Observe { generation: 7, .. }
            ),
            "stale gen-5 observation must be invalidated on advance"
        );
    }

    #[test]
    fn cancellation_fails_closed() {
        let mut s = specialist(AgentRole::Explorer);
        s.cancel();
        assert!(s.is_cancelled());
        assert_eq!(
            s.deliver(SpecialistMessage::Invalidate { generation: 1 }),
            Err(SpecialistError::Cancelled)
        );
        assert_eq!(s.retain_summary("x"), Err(SpecialistError::Cancelled));
        assert_eq!(s.mark_observed(1), Err(SpecialistError::Cancelled));
    }
}
