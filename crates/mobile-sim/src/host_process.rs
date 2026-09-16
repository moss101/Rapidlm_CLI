//! Shared bounded host-process runner.
//!
//! `android::manager`, `android::action`, `android::snapshot`, and
//! `ios::simctl` each hand-roll a "spawn, poll `try_wait`, read stdout once
//! it exits" runner against `adb`/`xcrun`. Stdout is drained here on a
//! background thread for the whole run, not read after the wait loop
//! observes exit: a child that writes more than the OS pipe buffer
//! (commonly 16-64 KiB, platform-dependent) before exiting blocks in
//! `write(2)` once that buffer fills, so it never reaches exit and gets
//! misreported as timed out even though it had already finished — a real
//! risk here specifically, since `adb exec-out screencap`/`uiautomator dump`
//! and `xcrun simctl list` output is routinely tens to hundreds of KiB. This
//! mirrors `crates/process-supervisor::cancel::await_exit_draining` locally,
//! since this crate does not depend on that one.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use capability_broker::CancellationToken;

/// Poll stride while waiting for the child to exit.
const HOST_POLL: Duration = Duration::from_millis(10);

/// Why a bounded host-process run failed. Callers map these to their own
/// local error type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostRunError {
    TimeoutInvalid,
    Spawn,
    Cancelled,
    Timeout,
    NonZeroExit,
    Wait,
    OutputTooLarge,
}

/// Runs `program` with `args`/`cwd`, bounded by `timeout`. Stdin is null,
/// stderr is discarded, stdout is captured and capped at `cap` bytes.
pub(crate) fn run_bounded_capturing_stdout<I, S>(
    program: &Path,
    args: I,
    cwd: Option<&Path>,
    timeout: Duration,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, HostRunError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if timeout.is_zero() {
        return Err(HostRunError::TimeoutInvalid);
    }
    if cancel.is_cancelled() {
        return Err(HostRunError::Cancelled);
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|_| HostRunError::Spawn)?;
    let stdout_pipe = child.stdout.take();
    let reader = stdout_pipe.map(|pipe| std::thread::spawn(move || drain_capped(pipe, cap)));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancel.is_cancelled() {
            kill_and_join(&mut child, reader);
            return Err(HostRunError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_and_join(&mut child, reader);
            return Err(HostRunError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(HOST_POLL),
            Err(_) => {
                kill_and_join(&mut child, reader);
                return Err(HostRunError::Wait);
            }
        }
    };

    let drained = reader
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    if !status.success() {
        return Err(HostRunError::NonZeroExit);
    }
    if drained.truncated {
        return Err(HostRunError::OutputTooLarge);
    }
    Ok(drained.bytes)
}

#[derive(Default)]
struct DrainedStdout {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Kills the child, reaps it, then joins the drain thread. Must kill/reap
/// first: the reader thread's blocking read only returns EOF once the
/// child's fds close, which happens when it exits.
fn kill_and_join(
    child: &mut std::process::Child,
    reader: Option<std::thread::JoinHandle<DrainedStdout>>,
) {
    let _ = child.kill();
    let _ = child.wait();
    if let Some(reader) = reader {
        let _ = reader.join();
    }
}

/// Reads `pipe` to EOF, keeping only the first `cap` bytes. Never stops
/// early on overflow: draining must continue for the whole stream or the
/// child could block on a full pipe exactly as before, just past `cap`
/// instead of past the (much smaller) OS pipe buffer.
fn drain_capped<R: Read>(mut pipe: R, cap: usize) -> DrainedStdout {
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    let mut truncated = false;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                let take = room.min(n);
                out.extend_from_slice(&buf[..take]);
                if take < n {
                    truncated = true;
                }
            }
        }
    }
    DrainedStdout {
        bytes: out,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require_bin(name: &str) -> std::path::PathBuf {
        test_fixtures::tool(name)
    }

    #[test]
    fn plain_echo_succeeds_and_captures_stdout() {
        let echo = require_bin("echo");
        let out = run_bounded_capturing_stdout(
            &echo,
            ["hello"],
            None,
            Duration::from_secs(5),
            1024,
            &CancellationToken::new(),
        )
        .expect("run");
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn nonzero_exit_is_reported() {
        let cmd = require_bin("false");
        let err = run_bounded_capturing_stdout(
            &cmd,
            std::iter::empty::<&str>(),
            None,
            Duration::from_secs(5),
            1024,
            &CancellationToken::new(),
        )
        .expect_err("nonzero exit");
        assert_eq!(err, HostRunError::NonZeroExit);
    }

    #[test]
    fn output_past_the_pipe_buffer_does_not_deadlock() {
        let dd = require_bin("dd");
        let out = run_bounded_capturing_stdout(
            &dd,
            ["if=/dev/zero", "bs=1024", "count=200"],
            None,
            Duration::from_secs(5),
            1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("run without deadlock");
        assert_eq!(out.len(), 200 * 1024);
        assert!(out.iter().all(|b| *b == 0));
    }

    #[test]
    fn truncation_at_a_small_cap_still_drains_without_deadlock() {
        let dd = require_bin("dd");
        let err = run_bounded_capturing_stdout(
            &dd,
            ["if=/dev/zero", "bs=1024", "count=200"],
            None,
            Duration::from_secs(5),
            64,
            &CancellationToken::new(),
        )
        .expect_err("truncated");
        assert_eq!(err, HostRunError::OutputTooLarge);
    }

    #[test]
    fn zero_timeout_is_rejected_before_spawning() {
        let echo = require_bin("echo");
        let err = run_bounded_capturing_stdout(
            &echo,
            ["hi"],
            None,
            Duration::ZERO,
            1024,
            &CancellationToken::new(),
        )
        .expect_err("zero timeout");
        assert_eq!(err, HostRunError::TimeoutInvalid);
    }

    #[test]
    fn already_cancelled_token_is_rejected_before_spawning() {
        let echo = require_bin("echo");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = run_bounded_capturing_stdout(
            &echo,
            ["hi"],
            None,
            Duration::from_secs(5),
            1024,
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, HostRunError::Cancelled);
    }
}
