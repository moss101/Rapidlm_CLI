//! Cross-process proof that `.rapidlm/goal-evidence.json` survives
//! concurrent writers without losing a committed record — the actual
//! production binary, several real OS processes, racing for real.
//!
//! `GoalHost::update_evidence`'s in-process, multi-threaded tests
//! (`goal_host.rs`) already prove the underlying `GoalLock` mechanism
//! serializes correctly; this file proves the same property holds when the
//! writers are genuinely separate `rapid` processes, not just separate
//! threads sharing one address space — mirroring `goal_concurrency.rs`'s
//! own cross-process proof for `goal.json`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-evidencerace-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A project directory with a real git repo (commit identity configured)
/// plus a trust record for it in the temp home's catalog — mirrors
/// `goal_concurrency.rs`'s own helper of the same shape. `goal`/`evidence`
/// subcommands don't actually check trust today, but this matches the
/// established fixture shape other integration tests already use.
fn trusted_project(home: &Path, project: &Path) {
    std::fs::create_dir_all(project).expect("project");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    for (key, value) in [("user.email", "evidencerace@example.com"), ("user.name", "evidencerace")]
    {
        let _ = Command::new("git")
            .args(["config", key, value])
            .current_dir(project)
            .output()
            .expect("git config");
    }
    let root = std::fs::canonicalize(project).expect("canonical root");
    let trust_dir = home.join(".rapidlm");
    std::fs::create_dir_all(&trust_dir).expect("rapidlm home");
    std::fs::write(
        trust_dir.join("project-trust.json"),
        format!(
            "{{\"schema\":1,\"records\":[{{\"canonical_root\":\"{}\",\"status\":\"trusted\"}}]}}",
            root.to_str().expect("utf-8 root")
        ),
    )
    .expect("trust catalog");
}

/// Run `rapid goal <args>` synchronously (setup/assertion steps — never
/// part of a timed race).
fn run_goal(project: &Path, home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut all: Vec<String> = vec!["goal".to_owned()];
    all.extend(args.iter().map(|s| s.to_string()));
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(&all)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid goal");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Spawn (but do not wait for) a real `rapid goal evidence record` child
/// process against `project`, distinguishable by `command`.
fn spawn_evidence_record(project: &Path, home: &Path, command: &str) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args([
            "goal",
            "evidence",
            "record",
            "--kind",
            "test",
            "--criterion",
            "c1",
            "--command",
            command,
        ])
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .spawn()
        .expect("spawn rapid goal evidence record")
}

fn read_evidence_doc(project: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(project.join(".rapidlm").join("goal-evidence.json"))
        .expect("goal-evidence.json must exist after every writer commits");
    serde_json::from_str(&raw)
        .expect("goal-evidence.json must be valid JSON — no torn/corrupt write")
}

/// Writer count for the race below. `goal_concurrency.rs`'s own cross-
/// process test found 2 writers insufficient to reliably overlap the real,
/// narrow read-modify-write window against ordinary inter-process
/// scheduling jitter, and 8 sufficient (`C(8,2) = 28` pairwise chances for
/// two writers to genuinely overlap instead of just one) — reused here for
/// the same reason, without needing to re-derive it.
const WRITERS: usize = 8;

#[test]
fn concurrent_rapid_goal_evidence_record_processes_all_commit_their_record() {
    let home = temp_dir("home");
    let project = home.join("project");
    trusted_project(&home, &project);

    // Setup: create the goal synchronously, before any race participant
    // starts — not itself part of the timed race.
    let (code, _out, err) = run_goal(&project, &home, &["create", "ship it", "--criterion", "c1=works"]);
    assert_eq!(code, Some(0), "goal create must succeed: {err}");

    // Launch every writer concurrently — never `.output()` (which would
    // block on one before the next even starts) — then wait for all.
    let commands: Vec<String> = (0..WRITERS).map(|i| format!("writer-{i}")).collect();
    let children: Vec<_> = commands
        .iter()
        .map(|command| spawn_evidence_record(&project, &home, command))
        .collect();
    for (i, mut child) in children.into_iter().enumerate() {
        let status = child.wait().unwrap_or_else(|err| panic!("wait for writer {i}: {err}"));
        assert!(status.success(), "writer {i} must exit cleanly");
    }

    let doc = read_evidence_doc(&project);
    let records = doc["records"].as_array().expect("records array");
    assert_eq!(
        records.len(),
        WRITERS,
        "every writer's own record must be present, not just whichever process saved last: {doc}"
    );
    let seen: std::collections::BTreeSet<&str> = records
        .iter()
        .map(|record| record["command"].as_str().expect("command"))
        .collect();
    let expected: std::collections::BTreeSet<&str> = commands.iter().map(String::as_str).collect();
    assert_eq!(seen, expected, "every writer's specific record must be the one that survived, \
        not duplicates of a subset");

    let _ = std::fs::remove_dir_all(&home);
}
