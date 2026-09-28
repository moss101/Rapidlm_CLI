//! The session and turn a spawned process belongs to (SEAM-12):
//! `RAPIDLM_SESSION_ID` and `RAPIDLM_TURN_ID`, exported to every process
//! RapidLM starts on a turn's behalf — a tool's command, a background job, a
//! hook, an MCP server (the session only: it outlives a turn), the status
//! command.
//!
//! Held per thread and set around the work that spawns, so two sessions in
//! one process (the daemon) never see each other's ids.

use std::cell::RefCell;

/// One run's identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunIdentity {
    pub session: String,
    pub turn: Option<String>,
}

thread_local! {
    static CURRENT: RefCell<Option<RunIdentity>> = const { RefCell::new(None) };
}

/// Run `work` with `identity` current on this thread (restored after).
pub fn scoped<T>(identity: Option<&RunIdentity>, work: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.replace(identity.cloned()));
    let out = work();
    CURRENT.with(|current| *current.borrow_mut() = previous);
    out
}

/// Make `identity` current on this thread until replaced — for a thread
/// that serves one run from start to end (a turn's own thread).
pub fn set_current(identity: Option<RunIdentity>) {
    CURRENT.with(|current| *current.borrow_mut() = identity);
}

/// The identity current on this thread.
pub fn current() -> Option<RunIdentity> {
    CURRENT.with(|current| current.borrow().clone())
}

/// The variables a process spawned now inherits.
pub fn env() -> Vec<(&'static str, String)> {
    let Some(identity) = current() else {
        return Vec::new();
    };
    let mut vars = vec![("RAPIDLM_SESSION_ID", identity.session)];
    if let Some(turn) = identity.turn {
        vars.push(("RAPIDLM_TURN_ID", turn));
    }
    vars
}

/// [`env`] without the turn: for a process that outlives it.
pub fn session_env() -> Vec<(&'static str, String)> {
    env()
        .into_iter()
        .filter(|(key, _)| *key == "RAPIDLM_SESSION_ID")
        .collect()
}
