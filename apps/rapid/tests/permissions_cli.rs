//! End-to-end coverage for `rapid permissions` against the **real compiled
//! binary**, with an isolated RapidLM home and project.
//!
//! What these prove beyond the in-crate suite: the command is dispatched at
//! all, its exit codes reach the process, a grant written by one invocation
//! is seen by the next, and the store the binary writes in a real home is the
//! one a real run reads.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
}

fn fixture(name: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "rapidlm-permcli-e2e-{name}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(home.join(".rapidlm")).expect("home");
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
    Fixture {
        root,
        home,
        project,
    }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Run {
        run_in(&self.project, &self.home, args)
    }

    /// `<home>/.rapidlm/project-permissions.json` — the store a real run
    /// reads, since `HOME=<home>` makes the binary resolve `<home>/.rapidlm`.
    fn store(&self) -> PathBuf {
        self.home.join(".rapidlm").join("project-permissions.json")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_in(cwd: &Path, home: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .env_remove("RAPIDLM_PERMISSION_MODE")
        .output()
        .expect("run rapid");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn the_command_is_dispatched_and_reports_an_empty_project() {
    let fixture = fixture("dispatch");
    let run = fixture.run(&["permissions", "list"]);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(
        !run.stderr.contains("unknown subcommand"),
        "not dispatched: {}",
        run.stderr
    );
    assert!(run.stdout.contains("grants=0"), "{}", run.stdout);
    assert!(run.stdout.contains("store="), "{}", run.stdout);
}

#[test]
fn a_grant_written_by_one_invocation_is_seen_by_the_next() {
    let fixture = fixture("roundtrip");
    let added = fixture.run(&["permissions", "allow", "workspace_write", "shell_exec(git *)"]);
    assert_eq!(added.code, Some(0), "stderr: {}", added.stderr);
    assert!(added.stdout.contains("allow=workspace_write"), "{}", added.stdout);

    let listed = fixture.run(&["permissions", "list"]);
    assert_eq!(listed.code, Some(0));
    assert!(listed.stdout.contains("grants=2"), "{}", listed.stdout);
    assert!(
        listed.stdout.contains("allow=shell_exec(git *)"),
        "an arg-glob pattern must round-trip through the store: {}",
        listed.stdout
    );

    // And the store really is the file a run reads.
    let text = std::fs::read_to_string(fixture.store()).expect("store written to the real home");
    assert!(text.contains("\"schema\""), "{text}");
    assert!(text.contains("shell_exec(git *)"), "{text}");

    let revoked = fixture.run(&["permissions", "revoke", "workspace_write"]);
    assert_eq!(revoked.code, Some(0), "{}", revoked.stdout);
    let listed = fixture.run(&["permissions", "list"]);
    assert!(listed.stdout.contains("grants=1"), "{}", listed.stdout);
}

#[test]
fn revoking_something_that_was_never_granted_exits_non_zero() {
    let fixture = fixture("revokemissing");
    let run = fixture.run(&["permissions", "revoke", "workspace_write"]);
    assert_eq!(run.code, Some(1), "{}", run.stdout);
    assert!(run.stdout.contains("not-granted=workspace_write"), "{}", run.stdout);
}

#[test]
fn a_bad_invocation_is_a_usage_error_on_stderr_with_nothing_on_stdout() {
    let fixture = fixture("usage");
    for args in [
        vec!["permissions", "bogus"],
        vec!["permissions", "allow"],
        vec!["permissions", "allow", "not a pattern"],
        vec!["permissions", "list", "extra"],
    ] {
        let run = fixture.run(&args);
        assert_eq!(run.code, Some(2), "{args:?} -> {}", run.stdout);
        assert!(run.stdout.is_empty(), "{args:?} wrote stdout: {}", run.stdout);
        assert!(
            run.stderr.contains("usage: rapid permissions"),
            "{args:?} stderr: {}",
            run.stderr
        );
    }
}

#[test]
fn an_untrusted_project_is_told_the_grant_changes_nothing_yet() {
    let fixture = fixture("untrusted");
    let run = fixture.run(&["permissions", "allow", "workspace_write"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stdout.contains("trust=untrusted"), "{}", run.stdout);
    assert!(run.stdout.contains("rapid trust grant"), "{}", run.stdout);

    // Granted, the note goes away — the grant now actually does something.
    let trusted = fixture.run(&["trust", "grant"]);
    assert_eq!(trusted.code, Some(0), "{}", trusted.stderr);
    let listed = fixture.run(&["permissions", "list"]);
    assert!(listed.stdout.contains("trust=trusted"), "{}", listed.stdout);
    assert!(
        !listed.stdout.contains("rapid trust grant"),
        "{}",
        listed.stdout
    );
}

#[test]
fn the_denial_a_user_actually_hits_names_this_command() {
    // `rapid permissions` exists because the default mode denies every
    // non-read call and nothing can prompt for approval. The user who hits
    // that denial has to be told the fix exists — an earlier version of this
    // test only asserted that the command's own help mentioned itself, which
    // could not fail, while the denial text said nothing about `rapid
    // permissions` at all.
    let denial = rapid::permissions::DecisionReason::ModeAsk.explanation();
    assert!(
        denial.contains("rapid permissions allow"),
        "the denial must name the command that fixes it: {denial}"
    );
    let ask_rule = rapid::permissions::DecisionReason::AskRule.explanation();
    assert!(
        ask_rule.contains("rapid permissions allow"),
        "an ask rule's denial must name it too: {ask_rule}"
    );

    // And the command it names really exists and really explains itself.
    let fixture = fixture("remedy");
    let help = fixture.run(&["permissions", "--help"]);
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    assert!(
        help.stdout
            .contains("suppresses the approval this build cannot yet prompt for"),
        "{}",
        help.stdout
    );
}
