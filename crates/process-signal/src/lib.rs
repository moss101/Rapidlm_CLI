//! Signal a whole process group — the one canonical way the workspace does it.
//!
//! Every job tree RapidLM starts is put in its own process group (pgid ==
//! leader pid) so that a timeout or a cancel can reach the grandchildren a
//! shell command forks, not just the leader. Five call sites — the
//! supervisor's cancel and recovery paths and the three sandbox backends —
//! used to do that by executing `/bin/kill -TERM -<pgid>`.
//!
//! On Linux `/bin/kill` is procps-ng's, and through procps-ng 4.0.4 (Ubuntu
//! 24.04) its parser handles an argument of the form `-<digits>` in the
//! `getopt` unknown-option case as `pid = '0' - <first digit>`: `-12345` is
//! `kill(-1, SIGTERM)`, the broadcast to every process the user owns. So on
//! Ubuntu every job timeout and every cancel signalled the user's whole
//! session, and the KILL escalation two hundred milliseconds later killed
//! it. BSD `kill(1)` — macOS — parses `-12345` as process group 12345, which
//! is why no developer machine ever saw it; GitHub's Ubuntu runner did, the
//! first time the plugin-host timeout test ran there: the runner itself was
//! signalled and the job ended with "The runner has received a shutdown
//! signal".
//!
//! `kill(2)` takes an integer. There is nothing to parse, nothing to find on
//! disk, and no process to exec while tearing another one down. This crate is
//! a leaf so that both the supervisor and the sandbox backends can share the
//! one implementation without either depending on the other.

use std::fmt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The two signals a termination sequence sends: `TERM` first, then `KILL`
/// after the grace period.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GroupSignal {
    Term,
    Kill,
}

/// Why a group could not be signalled. An absent group is not an error —
/// the tree is gone, which is what the caller wanted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SignalError {
    /// The id was below 2. `0` is the caller's own group and `1` is
    /// `init`'s, and a negated `1` is the broadcast — none is ever a job.
    InvalidGroup,
    /// The kernel refused: not permitted, or the platform call itself failed.
    Failed,
    /// No implementation for this platform.
    Unsupported,
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidGroup => "process group id below 2 is never a job",
            Self::Failed => "signalling the process group failed",
            Self::Unsupported => "process-group signals are unsupported on this platform",
        })
    }
}

impl std::error::Error for SignalError {}

/// Lowest process group id that can ever name a job. Below it are the
/// caller's own group (`0`), `init` (`1`) and — negated — the broadcast.
pub const MIN_GROUP_ID: u32 = 2;

/// Send `signal` to every process in group `pgid`.
///
/// `Ok(())` when the signal was delivered *or the group no longer exists*
/// (`ESRCH`; on Windows, `taskkill` exit 128): either way nothing is left
/// to stop. `Err(SignalError::InvalidGroup)` for any id below
/// [`MIN_GROUP_ID`], checked before any platform call.
pub fn signal_process_group(pgid: u32, signal: GroupSignal) -> Result<(), SignalError> {
    if pgid < MIN_GROUP_ID {
        return Err(SignalError::InvalidGroup);
    }
    platform::signal_group(pgid, signal)
}

/// Put the child `command` will spawn in its own process group — pgid ==
/// pid on Unix, a new group on Windows — so that a group signal reaches
/// exactly the tree it forks and nothing else. The counterpart of
/// [`signal_process_group`]: without it the child shares the caller's
/// group, and the "group" to signal would be the caller's own.
pub fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

/// Grace between the group `TERM` and the group `KILL` in
/// [`terminate_process_group`]: long enough for a cooperative child to
/// flush and exit, short enough that a cancel feels immediate.
pub const DEFAULT_TERM_GRACE: Duration = Duration::from_millis(80);

/// How long [`terminate_process_group`] waits for the leader to exit after
/// the group `KILL` before falling back to reaping the leader alone.
pub const DEFAULT_KILL_WAIT: Duration = Duration::from_secs(2);

/// Poll stride while waiting on the leader in [`terminate_process_group`].
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Stop a child spawned under [`isolate_process_group`] and everything it
/// forked: `TERM` the group, wait up to `grace` for the leader to exit,
/// then `KILL` the group *unconditionally* — a grandchild that trapped
/// `TERM` (`trap "" TERM; sleep 10`) is still in the group after the
/// leader has gone, and the group id stays valid while any member lives —
/// wait up to `kill_wait`, and reap the leader. Whatever happens in
/// between, the leader is killed and reaped before this returns, so the
/// caller never leaves a zombie. Best-effort throughout: a signal failure
/// is not an error here, since the leader is reaped regardless.
pub fn terminate_process_group(child: &mut Child, grace: Duration, kill_wait: Duration) {
    let pid = child.id();
    if pid >= MIN_GROUP_ID {
        let _ = signal_process_group(pid, GroupSignal::Term);
        let _ = wait_leader(child, grace);
        let _ = signal_process_group(pid, GroupSignal::Kill);
        if wait_leader(child, kill_wait) {
            return;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// [`terminate_process_group`] with [`DEFAULT_TERM_GRACE`] and
/// [`DEFAULT_KILL_WAIT`].
pub fn terminate_process_group_default(child: &mut Child) {
    terminate_process_group(child, DEFAULT_TERM_GRACE, DEFAULT_KILL_WAIT);
}

/// `true` once the leader has been reaped within `budget`; `false` on the
/// deadline or a `try_wait` error (the caller then escalates).
fn wait_leader(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => return false,
        }
    }
    false
}

/// Whether a process with this pid exists — `kill(pid, 0)`, which delivers
/// nothing. `EPERM` means it exists and belongs to someone else, so that is
/// `true` too; only `ESRCH` is `false`. Pids below [`MIN_GROUP_ID`] are
/// `false` without a call. Always `false` on platforms without `kill(2)`.
pub fn process_exists(pid: u32) -> bool {
    if pid < MIN_GROUP_ID {
        return false;
    }
    platform::process_exists(pid)
}

#[cfg(unix)]
mod platform {
    use super::{GroupSignal, SignalError};
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group, test_kill_process};

    pub(super) fn process_exists(pid: u32) -> bool {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        let Some(pid) = Pid::from_raw(raw) else {
            return false;
        };
        !matches!(test_kill_process(pid), Err(Errno::SRCH))
    }

    pub(super) fn signal_group(pgid: u32, signal: GroupSignal) -> Result<(), SignalError> {
        let raw = i32::try_from(pgid).map_err(|_| SignalError::InvalidGroup)?;
        // `from_raw` refuses 0; 1 was refused above. What reaches the kernel
        // is `kill(-pgid, sig)` for this pgid and no other.
        let pid = Pid::from_raw(raw).ok_or(SignalError::InvalidGroup)?;
        let sig = match signal {
            GroupSignal::Term => Signal::TERM,
            GroupSignal::Kill => Signal::KILL,
        };
        match kill_process_group(pid, sig) {
            Ok(()) => Ok(()),
            Err(Errno::SRCH) => Ok(()),
            Err(_) => Err(SignalError::Failed),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::process::{Command, Stdio};

    use super::{GroupSignal, SignalError};

    /// Absolute `taskkill.exe` path; never PATH-searched.
    const TASKKILL_PROGRAM: &str = r"C:\Windows\System32\taskkill.exe";

    pub(super) fn process_exists(_pid: u32) -> bool {
        // No `kill(pid, 0)` here; recovery on Windows does not poll liveness
        // this way today, and a wrong `true` would stall a wait, so `false`.
        false
    }

    pub(super) fn signal_group(pgid: u32, signal: GroupSignal) -> Result<(), SignalError> {
        // Spawn recorded the leader pid as the group id
        // (`CREATE_NEW_PROCESS_GROUP`). `/T` terminates descendants; `/F` is
        // the hard-kill escalation.
        let pid = pgid.to_string();
        let mut command = Command::new(TASKKILL_PROGRAM);
        command.args(["/PID", &pid, "/T"]);
        if matches!(signal, GroupSignal::Kill) {
            command.arg("/F");
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .status()
            .map_err(|_| SignalError::Failed)?;
        // 128 is taskkill's "process not found".
        if status.success() || status.code() == Some(128) {
            Ok(())
        } else {
            Err(SignalError::Failed)
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{GroupSignal, SignalError};

    pub(super) fn process_exists(_pid: u32) -> bool {
        false
    }

    pub(super) fn signal_group(_pgid: u32, _signal: GroupSignal) -> Result<(), SignalError> {
        Err(SignalError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_broadcast_and_init_groups_are_refused_before_any_syscall() {
        for pgid in [0, 1] {
            assert_eq!(
                signal_process_group(pgid, GroupSignal::Term),
                Err(SignalError::InvalidGroup)
            );
            assert_eq!(
                signal_process_group(pgid, GroupSignal::Kill),
                Err(SignalError::InvalidGroup)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_exists_sees_this_process_and_not_a_reaped_one() {
        use std::process::{Command, Stdio};
        assert!(process_exists(std::process::id()));
        // 0 and 1 are refused before any call: neither is ever a job.
        assert!(!process_exists(0));
        assert!(!process_exists(1));
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("child");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(
            !process_exists(pid),
            "a reaped child still reported as alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_reaches_a_grandchild_that_traps_term() {
        // The leader exits on TERM at once; its grandchild ignores TERM and
        // would outlive a terminate that stopped once the leader was gone.
        // The group KILL after the grace is what reaches it. The grandchild
        // writes its pid to a file so the test can watch it die.
        use std::io::BufRead;
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!(
            "rapidlm-process-signal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = "(trap '' TERM; echo $$; exec sleep 30) & echo started; wait";
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        isolate_process_group(&mut command);
        let mut leader = command.spawn().expect("leader");
        let mut lines = std::io::BufReader::new(leader.stdout.take().expect("stdout")).lines();
        let mut grandchild: Option<u32> = None;
        for _ in 0..2 {
            let line = lines.next().expect("a line").expect("read");
            if let Ok(pid) = line.trim().parse::<u32>() {
                grandchild = Some(pid);
            }
        }
        let grandchild = grandchild.expect("grandchild pid");
        assert!(process_exists(grandchild));

        let started = Instant::now();
        terminate_process_group(
            &mut leader,
            Duration::from_millis(80),
            Duration::from_secs(5),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "terminate waited the whole kill budget: {:?}",
            started.elapsed()
        );
        let gone = Instant::now() + Duration::from_secs(5);
        while process_exists(grandchild) {
            assert!(
                Instant::now() < gone,
                "grandchild that trapped TERM survived the group KILL"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_that_no_longer_exists_is_not_an_error() {
        // A pgid this large is not a live group on any test host; `ESRCH`
        // is "nothing left to stop", which is success for a terminator.
        assert_eq!(signal_process_group(0x3fff_fff0, GroupSignal::Term), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_named_group_is_signalled() {
        // The bug this crate replaces: `/bin/kill -TERM -<pgid>` on procps-ng
        // was `kill(-1, SIGTERM)` — every process the user owns, this test
        // included. The proof is a bystander in its own group that must
        // survive, next to a leader-plus-grandchild group that must not.
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let sh = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|p| std::path::Path::new(p).is_file())
            .expect("a shell");
        let mut bystander = Command::new(sh)
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("bystander");
        // The leader forks a grandchild and waits on it, so the group holds
        // two processes and only a *group* signal reaches the second. It
        // says `ready` only after the fork: a signal sent before the
        // grandchild exists would make the group-empty check below pass
        // for a leader-only kill too (revert cycle 148 found exactly that).
        let mut leader = Command::new(sh)
            .args(["-c", "sleep 30 & echo ready; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("leader");
        let group = leader.id();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(leader.stdout.take().expect("piped stdout")),
            &mut ready,
        )
        .expect("read ready");
        assert_eq!(ready.trim(), "ready");

        signal_process_group(group, GroupSignal::Term).expect("signal");

        let deadline = Instant::now() + Duration::from_secs(5);
        let leader_status = loop {
            if let Some(status) = leader.try_wait().expect("try_wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "leader outlived SIGTERM to its group"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            !leader_status.success(),
            "leader was signalled, not exited: {leader_status:?}"
        );
        // The grandchild was reached only if the *group* was signalled:
        // `kill(-pgid, 0)` keeps succeeding while any member is alive and
        // fails with `ESRCH` once the group is empty.
        let grandchild_gone = Instant::now() + Duration::from_secs(5);
        let pid =
            rustix::process::Pid::from_raw(i32::try_from(group).expect("pid fits")).expect("pid");
        while rustix::process::test_kill_process_group(pid).is_ok() {
            assert!(
                Instant::now() < grandchild_gone,
                "grandchild outlived SIGTERM to its group"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The bystander is untouched, and so is this process: we are here.
        assert!(
            bystander.try_wait().expect("try_wait").is_none(),
            "a signal to one group reached a process outside it"
        );
        let _ = bystander.kill();
        let _ = bystander.wait();
    }
}
