//! Process-group accounting shared by the process-based backends: which
//! pids are in a group, and how much resident memory they hold, read through
//! `pgrep(1)`/`ps(1)` at absolute paths. Every backend that isolates its
//! child into a group samples it here; the backends used to carry three
//! copies of this, and one of them differed — see [`sample_process_group`].

use std::path::Path;
use std::process::{Command, Stdio};

/// Absolute `ps(1)` paths; never PATH-searched.
const PS_PROGRAMS: &[&str] = &["/bin/ps", "/usr/bin/ps"];

/// Absolute `pgrep(1)` paths; never PATH-searched.
const PGREP_PROGRAMS: &[&str] = &["/usr/bin/pgrep", "/bin/pgrep"];

/// The first of `candidates` that exists as a regular file, for tables of
/// absolute program paths.
pub(crate) fn first_existing(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
}

/// Whether this host can account a process group at all — `pgrep` or `ps`
/// is present — which the backends require at health time rather than
/// discovering at the first sample.
pub(crate) fn group_accounting_available() -> bool {
    first_existing(PS_PROGRAMS).is_some() || first_existing(PGREP_PROGRAMS).is_some()
}

/// `(pids, memory_mb)` for the process group `pgid`. `None` when the group
/// cannot be listed or is empty (an id below 2 is never a job's).
///
/// A pid that is listed and then gone before its rss is read — a
/// grandchild exiting between the two commands, ordinary under process
/// churn — counts as 0, not as a failed sample: dropping the whole sample
/// for one vanished pid (which the gvisor copy did) skips a memory-limit
/// tick exactly when the tree is busiest.
pub(crate) fn sample_process_group(pgid: u32) -> Option<(u32, u64)> {
    if pgid < 2 {
        return None;
    }
    let pids = group_pids(pgid)?;
    if pids.is_empty() {
        return None;
    }
    let count = u32::try_from(pids.len()).unwrap_or(u32::MAX);
    let mut rss_kb = 0u64;
    for pid in &pids {
        rss_kb = rss_kb.saturating_add(pid_rss_kb(*pid).unwrap_or(0));
    }
    let memory_mb = rss_kb.div_ceil(1024);
    Some((count, memory_mb))
}

fn group_pids(pgid: u32) -> Option<Vec<u32>> {
    if let Some(pids) = pgrep_group(pgid) {
        return Some(pids);
    }
    ps_group(pgid)
}

fn pgrep_group(pgid: u32) -> Option<Vec<u32>> {
    let program = first_existing(PGREP_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-g", &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(pid) = line.trim().parse::<u32>()
            && pid >= 2
        {
            pids.push(pid);
        }
    }
    if pids.is_empty() { None } else { Some(pids) }
}

fn ps_group(pgid: u32) -> Option<Vec<u32>> {
    let program = first_existing(PS_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-ax", "-o", "pid=,pgid="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut cols = line.split_whitespace();
        let Some(pid) = cols.next().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let Some(group) = cols.next().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        if group == pgid && pid >= 2 {
            pids.push(pid);
        }
    }
    if pids.is_empty() { None } else { Some(pids) }
}

fn pid_rss_kb(pid: u32) -> Option<u64> {
    let program = first_existing(PS_PROGRAMS)?;
    let output = Command::new(program)
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|col| col.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_below_two_are_never_sampled() {
        assert_eq!(sample_process_group(0), None);
        assert_eq!(sample_process_group(1), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_with_a_vanished_member_is_still_a_sample() {
        // A leader that forks a short-lived grandchild and keeps running: the
        // listing may include the grandchild, the rss read may then miss it,
        // and the sample must still come back with the leader counted.
        let sh = first_existing(&["/bin/sh", "/usr/bin/sh"]).expect("a shell");
        let mut command = Command::new(sh);
        command
            .args(["-c", "(exit 0) & sleep 2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_signal::isolate_process_group(&mut command);
        let mut leader = command.spawn().expect("leader");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let sample = loop {
            if let Some(sample) = sample_process_group(leader.id()) {
                break sample;
            }
            assert!(std::time::Instant::now() < deadline, "group never sampled");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = leader.kill();
        let _ = leader.wait();
        assert!(
            sample.0 >= 1,
            "the leader itself is in its group: {sample:?}"
        );
    }
}
