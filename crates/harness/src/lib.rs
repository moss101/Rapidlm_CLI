//! P11 eval-harness core: scenarios/fixtures, deterministic drivers,
//! kernel-backed runs, fault injection, assertions, graders, metrics.

pub mod assertions;
pub mod drivers;
pub mod faults;
pub mod graders;
pub mod scenario;

/// Wire/schema version for all P11 harness records.
pub const EVAL_SCHEMA: u16 = 1;

/// Maximum fixtures retained by the CAS store.
pub const MAX_FIXTURES: usize = 256;
