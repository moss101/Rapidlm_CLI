//! Single error-resilient stderr writer for headless exec diagnostics.
//!
//! `eprintln!` panics when the stderr write fails; inside a tool-batch
//! worker thread that panic silently kills the whole batch (the dispatcher
//! drops the dead thread's outcomes). Exec diagnostics must never take the
//! run down or vanish into a dead worker: every line goes through
//! [`stderr_line`], which ignores write errors and reports whether the line
//! was emitted.

use std::io::Write;

/// Write one line to stderr. Never panics; returns `false` when the write
/// failed so callers can count or surface the loss.
pub fn stderr_line(text: &str) -> bool {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{text}").is_ok()
}
