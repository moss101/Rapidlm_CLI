//! Own interactive terminal modes and restore them on every exit path.
//!
//! Interactive TUI owns stdout. Headless JSONL also owns stdout, so this
//! module refuses to enter raw/alternate-screen modes for a headless frontend
//! (`docs/api-contracts/headless-jsonl.md`).
//!
//! A panic hook restores modes before the previous hook prints the crash so
//! the user's shell is not left raw.

use std::fmt::{self, Debug, Display, Formatter};
use std::io::{self, IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::{Arc, Mutex, OnceLock};

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Which frontend is requesting terminal ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrontendKind {
    /// Ratatui/crossterm interactive session. Stdout belongs to the TUI.
    Interactive,
    /// JSONL protocol on stdout. Must never take over the terminal.
    Headless,
}

/// Discrete mode mutations applied to a [`TerminalBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalOp {
    EnterRawMode,
    LeaveRawMode,
    EnterAlternateScreen,
    LeaveAlternateScreen,
    HideCursor,
    ShowCursor,
}

impl TerminalOp {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::EnterRawMode => "enter_raw_mode",
            Self::LeaveRawMode => "leave_raw_mode",
            Self::EnterAlternateScreen => "enter_alternate_screen",
            Self::LeaveAlternateScreen => "leave_alternate_screen",
            Self::HideCursor => "hide_cursor",
            Self::ShowCursor => "show_cursor",
        }
    }
}

/// Typed failures for terminal acquire/restore. No silent fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalError {
    /// Headless JSONL reserves stdout; raw mode would mix protocol bytes.
    HeadlessStdoutReserved,
    /// Another non-restored guard still owns the process terminal.
    AlreadyActive,
    /// Production backend refused because stdout is not a TTY.
    NotATty,
    /// Backend rejected a mode change.
    Backend { op: TerminalOp, message: String },
}

impl Display for TerminalError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadlessStdoutReserved => {
                f.write_str("headless frontend reserves stdout; refusing terminal takeover")
            }
            Self::AlreadyActive => f.write_str("terminal modes are already owned"),
            Self::NotATty => f.write_str("stdout is not a tty"),
            Self::Backend { op, message } => {
                write!(f, "terminal {} failed: {message}", op.as_label())
            }
        }
    }
}

impl std::error::Error for TerminalError {}

/// Applies [`TerminalOp`] values. Production uses crossterm; tests record.
pub trait TerminalBackend: Send {
    fn apply(&mut self, op: TerminalOp) -> Result<(), TerminalError>;
}

/// Crossterm-backed writer used by the interactive TUI.
pub struct CrosstermBackend<W> {
    out: W,
}

impl CrosstermBackend<io::Stdout> {
    pub fn stdout() -> Self {
        Self { out: io::stdout() }
    }
}

impl<W: Write + Send> TerminalBackend for CrosstermBackend<W> {
    fn apply(&mut self, op: TerminalOp) -> Result<(), TerminalError> {
        let io_err = match op {
            TerminalOp::EnterRawMode => enable_raw_mode().err(),
            TerminalOp::LeaveRawMode => disable_raw_mode().err(),
            TerminalOp::EnterAlternateScreen => execute!(self.out, EnterAlternateScreen).err(),
            TerminalOp::LeaveAlternateScreen => execute!(self.out, LeaveAlternateScreen).err(),
            TerminalOp::HideCursor => execute!(self.out, Hide).err(),
            TerminalOp::ShowCursor => execute!(self.out, Show).err(),
        };
        match io_err {
            None => Ok(()),
            Some(err) => Err(TerminalError::Backend {
                op,
                message: err.to_string(),
            }),
        }
    }
}

/// In-memory backend that records ops for snapshot/integration tests.
#[derive(Clone)]
pub struct RecordingBackend {
    log: Arc<Mutex<Vec<TerminalOp>>>,
    fail_on: Option<TerminalOp>,
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        }
    }

    pub fn fail_on(op: TerminalOp) -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            fail_on: Some(op),
        }
    }

    pub fn with_log(log: Arc<Mutex<Vec<TerminalOp>>>) -> Self {
        Self { log, fail_on: None }
    }

    pub fn snapshot(&self) -> Vec<TerminalOp> {
        lock_vec(&self.log).clone()
    }

    pub fn snapshot_text(&self) -> String {
        snapshot_text(&self.snapshot())
    }
}

impl Default for RecordingBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for RecordingBackend {
    fn apply(&mut self, op: TerminalOp) -> Result<(), TerminalError> {
        lock_vec(&self.log).push(op);
        if self.fail_on == Some(op) {
            return Err(TerminalError::Backend {
                op,
                message: "injected failure".to_owned(),
            });
        }
        Ok(())
    }
}

/// RAII owner of raw mode, alternate screen, and cursor visibility.
///
/// [`Drop`] restores modes. A process-wide panic hook is the crash fallback
/// and runs before the previous hook so crash output is readable.
#[must_use = "dropping TerminalGuard restores the terminal"]
pub struct TerminalGuard {
    inner: Arc<Mutex<GuardState>>,
}

struct GuardState {
    backend: Box<dyn TerminalBackend>,
    raw: bool,
    alt: bool,
    cursor_hidden: bool,
    restored: bool,
}

static ARMED: Mutex<Option<Arc<Mutex<GuardState>>>> = Mutex::new(None);
static HOOK: OnceLock<()> = OnceLock::new();

const ENTER_OPS: [TerminalOp; 3] = [
    TerminalOp::EnterRawMode,
    TerminalOp::EnterAlternateScreen,
    TerminalOp::HideCursor,
];

impl TerminalGuard {
    /// Acquire interactive modes, or refuse headless stdout takeover.
    pub fn acquire(kind: FrontendKind) -> Result<Self, TerminalError> {
        match kind {
            FrontendKind::Headless => Err(TerminalError::HeadlessStdoutReserved),
            FrontendKind::Interactive => {
                if !io::stdout().is_terminal() {
                    return Err(TerminalError::NotATty);
                }
                Self::enter(Box::new(CrosstermBackend::stdout()))
            }
        }
    }

    /// Acquire using an injected backend. Headless still never touches it.
    pub fn acquire_with<B>(kind: FrontendKind, backend: B) -> Result<Self, TerminalError>
    where
        B: TerminalBackend + 'static,
    {
        match kind {
            FrontendKind::Headless => Err(TerminalError::HeadlessStdoutReserved),
            FrontendKind::Interactive => Self::enter(Box::new(backend)),
        }
    }

    fn enter(backend: Box<dyn TerminalBackend>) -> Result<Self, TerminalError> {
        install_panic_hook();
        let inner = Arc::new(Mutex::new(GuardState {
            backend,
            raw: false,
            alt: false,
            cursor_hidden: false,
            restored: false,
        }));
        try_arm(&inner)?;

        let apply_result = {
            let mut state = lock_state(&inner);
            apply_enter(&mut state)
        };
        if let Err(err) = apply_result {
            let mut state = lock_state(&inner);
            let _ = state.restore();
            disarm(&inner);
            return Err(err);
        }
        Ok(Self { inner })
    }

    /// Restore modes now. Idempotent with [`Drop`] and the panic hook.
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        let result = {
            let mut state = lock_state(&self.inner);
            state.restore()
        };
        if result.is_ok() {
            disarm(&self.inner);
        }
        result
    }

    pub fn is_restored(&self) -> bool {
        lock_state(&self.inner).restored
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl Debug for TerminalGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = lock_state(&self.inner);
        f.debug_struct("TerminalGuard")
            .field("raw", &state.raw)
            .field("alt", &state.alt)
            .field("cursor_hidden", &state.cursor_hidden)
            .field("restored", &state.restored)
            .finish()
    }
}

impl GuardState {
    fn restore(&mut self) -> Result<(), TerminalError> {
        if self.restored {
            return Ok(());
        }
        let mut first = None;
        if self.cursor_hidden {
            match self.backend.apply(TerminalOp::ShowCursor) {
                Ok(()) => self.cursor_hidden = false,
                Err(err) => {
                    first.get_or_insert(err);
                }
            }
        }
        if self.alt {
            match self.backend.apply(TerminalOp::LeaveAlternateScreen) {
                Ok(()) => self.alt = false,
                Err(err) => {
                    first.get_or_insert(err);
                }
            }
        }
        if self.raw {
            match self.backend.apply(TerminalOp::LeaveRawMode) {
                Ok(()) => self.raw = false,
                Err(err) => {
                    first.get_or_insert(err);
                }
            }
        }
        if first.is_none() {
            self.restored = true;
        }
        match first {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn apply_enter(state: &mut GuardState) -> Result<(), TerminalError> {
    for op in ENTER_OPS {
        state.backend.apply(op)?;
        match op {
            TerminalOp::EnterRawMode => state.raw = true,
            TerminalOp::EnterAlternateScreen => state.alt = true,
            TerminalOp::HideCursor => state.cursor_hidden = true,
            TerminalOp::LeaveRawMode
            | TerminalOp::LeaveAlternateScreen
            | TerminalOp::ShowCursor => {}
        }
    }
    Ok(())
}

fn snapshot_text(ops: &[TerminalOp]) -> String {
    let mut out = String::new();
    for (i, op) in ops.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(op.as_label());
    }
    out
}

fn lock_vec(log: &Mutex<Vec<TerminalOp>>) -> std::sync::MutexGuard<'_, Vec<TerminalOp>> {
    log.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn lock_state(inner: &Mutex<GuardState>) -> std::sync::MutexGuard<'_, GuardState> {
    inner.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn lock_armed() -> std::sync::MutexGuard<'static, Option<Arc<Mutex<GuardState>>>> {
    ARMED.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn try_arm(inner: &Arc<Mutex<GuardState>>) -> Result<(), TerminalError> {
    let mut slot = lock_armed();
    if let Some(current) = slot.as_ref()
        && !lock_state(current).restored
    {
        return Err(TerminalError::AlreadyActive);
    }
    *slot = Some(Arc::clone(inner));
    Ok(())
}

fn disarm(inner: &Arc<Mutex<GuardState>>) {
    let mut slot = lock_armed();
    if let Some(current) = slot.as_ref()
        && Arc::ptr_eq(current, inner)
    {
        *slot = None;
    }
}

#[cfg(test)]
fn armed_is_live() -> bool {
    match lock_armed().as_ref() {
        Some(inner) => !lock_state(inner).restored,
        None => false,
    }
}

/// Restore the armed guard if any. Used by the panic hook and crash paths.
pub fn restore_if_armed() -> bool {
    let Some(inner) = lock_armed().clone() else {
        return false;
    };
    let result = {
        let mut state = lock_state(&inner);
        state.restore()
    };
    if result.is_ok() {
        disarm(&inner);
    }
    result.is_ok()
}

fn install_panic_hook() {
    HOOK.get_or_init(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            let _ = restore_if_armed();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ENTER_RESTORE_GOLDEN: &str = "\
enter_raw_mode
enter_alternate_screen
hide_cursor
show_cursor
leave_alternate_screen
leave_raw_mode";

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn begin_test() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = restore_if_armed();
        guard
    }

    #[test]
    fn drop_restores_snapshot() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        {
            let _guard = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)
                .expect("enter interactive");
            assert_eq!(
                log.snapshot_text(),
                "enter_raw_mode\nenter_alternate_screen\nhide_cursor"
            );
        }
        assert_eq!(log.snapshot_text(), ENTER_RESTORE_GOLDEN);
    }

    #[test]
    fn explicit_restore_is_idempotent_with_drop() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let mut guard = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)
            .expect("enter interactive");
        guard.restore().expect("restore");
        assert!(guard.is_restored());
        guard.restore().expect("second restore");
        drop(guard);
        assert_eq!(log.snapshot_text(), ENTER_RESTORE_GOLDEN);
    }

    #[test]
    fn normal_error_path_restores() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let err = (|| -> Result<(), TerminalError> {
            let _guard = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)?;
            Err(TerminalError::Backend {
                op: TerminalOp::LeaveRawMode,
                message: "kernel disconnect".to_owned(),
            })
        })()
        .expect_err("session error");
        assert!(matches!(err, TerminalError::Backend { .. }));
        assert_eq!(log.snapshot_text(), ENTER_RESTORE_GOLDEN);
    }

    #[test]
    fn panic_drop_and_hook_restore_once() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)
                .expect("enter interactive");
            panic!("induced terminal panic");
        }));
        assert!(result.is_err(), "panic must propagate");
        assert_eq!(log.snapshot_text(), ENTER_RESTORE_GOLDEN);
        assert!(
            !armed_is_live(),
            "panic path must disarm so a later session can enter"
        );
    }

    #[test]
    fn panic_hook_restores_when_drop_is_skipped() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let guard = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)
            .expect("enter interactive");
        std::mem::forget(guard);
        assert!(
            restore_if_armed(),
            "hook fallback must restore a leaked guard"
        );
        assert_eq!(log.snapshot_text(), ENTER_RESTORE_GOLDEN);
        assert!(!armed_is_live());
    }

    #[test]
    fn headless_refuses_without_backend_ops() {
        let _lock = begin_test();
        let backend = RecordingBackend::new();
        let log = backend.clone();
        let err = TerminalGuard::acquire_with(FrontendKind::Headless, backend)
            .expect_err("headless must fail closed");
        assert_eq!(err, TerminalError::HeadlessStdoutReserved);
        assert!(
            log.snapshot().is_empty(),
            "headless must not write terminal control to stdout"
        );
        let err = TerminalGuard::acquire(FrontendKind::Headless)
            .expect_err("headless acquire must fail closed");
        assert_eq!(err, TerminalError::HeadlessStdoutReserved);
    }

    #[test]
    fn interactive_stdout_acquire_fails_closed_without_tty() {
        let _lock = begin_test();
        if io::stdout().is_terminal() {
            return;
        }
        let err = TerminalGuard::acquire(FrontendKind::Interactive)
            .expect_err("non-tty stdout must not enter raw mode");
        assert_eq!(err, TerminalError::NotATty);
    }

    #[test]
    fn enter_failure_rolls_back_partial_modes() {
        let _lock = begin_test();
        let log = Arc::new(Mutex::new(Vec::new()));
        let backend = RecordingBackend {
            log: log.clone(),
            fail_on: Some(TerminalOp::EnterAlternateScreen),
        };
        let err = TerminalGuard::acquire_with(FrontendKind::Interactive, backend)
            .expect_err("alt-screen failure");
        assert!(matches!(
            err,
            TerminalError::Backend {
                op: TerminalOp::EnterAlternateScreen,
                ..
            }
        ));
        assert_eq!(
            snapshot_text(&lock_vec(&log)),
            "enter_raw_mode\nenter_alternate_screen\nleave_raw_mode"
        );
        assert!(!armed_is_live());
    }

    #[test]
    fn second_guard_is_rejected_until_restore() {
        let _lock = begin_test();
        let first_backend = RecordingBackend::new();
        let second_backend = RecordingBackend::new();
        let second_log = second_backend.clone();
        let first = TerminalGuard::acquire_with(FrontendKind::Interactive, first_backend)
            .expect("first guard");
        let err = TerminalGuard::acquire_with(FrontendKind::Interactive, second_backend)
            .expect_err("second guard");
        assert_eq!(err, TerminalError::AlreadyActive);
        assert!(second_log.snapshot().is_empty());
        drop(first);
        let third = RecordingBackend::new();
        drop(
            TerminalGuard::acquire_with(FrontendKind::Interactive, third)
                .expect("enter after restore"),
        );
    }

    #[test]
    fn restore_keeps_trying_after_partial_restore_failure() {
        let _lock = begin_test();
        let attempts = Arc::new(AtomicUsize::new(0));
        struct FlakyLeave {
            attempts: Arc<AtomicUsize>,
            log: Arc<Mutex<Vec<TerminalOp>>>,
        }
        impl TerminalBackend for FlakyLeave {
            fn apply(&mut self, op: TerminalOp) -> Result<(), TerminalError> {
                lock_vec(&self.log).push(op);
                if op == TerminalOp::LeaveAlternateScreen
                    && self.attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return Err(TerminalError::Backend {
                        op,
                        message: "leave alt failed".to_owned(),
                    });
                }
                Ok(())
            }
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut guard = TerminalGuard::acquire_with(
            FrontendKind::Interactive,
            FlakyLeave {
                attempts: attempts.clone(),
                log: log.clone(),
            },
        )
        .expect("enter");
        let err = guard.restore().expect_err("first restore incomplete");
        assert!(matches!(
            err,
            TerminalError::Backend {
                op: TerminalOp::LeaveAlternateScreen,
                ..
            }
        ));
        assert!(!guard.is_restored());
        guard.restore().expect("retry restore");
        assert!(guard.is_restored());
        assert_eq!(
            snapshot_text(&lock_vec(&log)),
            "enter_raw_mode\nenter_alternate_screen\nhide_cursor\n\
             show_cursor\nleave_alternate_screen\nleave_raw_mode\n\
             leave_alternate_screen"
        );
    }
}
