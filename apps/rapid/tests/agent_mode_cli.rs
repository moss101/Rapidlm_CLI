//! SEAM-10: agent-mode flags and the JSON error envelope, end to end.

use std::process::Command;

fn rapid(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let home = std::env::temp_dir().join(format!("rapid-agent-mode-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(args)
        .current_dir(&home)
        .env("HOME", &home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_OUTPUT")
        .env_remove("RAPIDLM_DRY_RUN")
        .env_remove("RAPIDLM_NON_INTERACTIVE");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run rapid")
}

#[test]
fn a_failure_in_json_mode_is_the_envelope_and_keeps_its_exit_code() {
    for (args, env) in [
        (vec!["--output", "json", "nosuchcmd"], vec![]),
        (vec!["nosuchcmd"], vec![("RAPIDLM_OUTPUT", "json")]),
    ] {
        let out = rapid(&args, &env);
        assert_eq!(out.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr.lines().last().expect("a line");
        let doc: serde_json::Value = serde_json::from_str(line).expect(line);
        assert_eq!(doc["error"]["code"], "usage");
        assert!(
            doc["error"]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("rapid --help"))
        );
    }
    // Text mode: the message, then the hint; same exit code.
    let out = rapid(&["nosuchcmd"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("hint: `rapid --help`"));
}

#[test]
fn a_flag_a_command_cannot_honour_is_refused_before_it_runs() {
    let out = rapid(&["--dry-run", "trust", "grant"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("has no dry run"));
    let out = rapid(&["--non-interactive"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let out = rapid(&["resume"], &[("RAPIDLM_NON_INTERACTIVE", "1")]);
    assert_eq!(out.status.code(), Some(2));
    // A read-only command takes --dry-run as nothing to not do.
    let out = rapid(&["--dry-run", "completions", "bash"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        rapid(&["--timeout", "never", "tools"], &[]).status.code(),
        Some(2)
    );
}

#[test]
fn a_timeout_stops_the_run_with_the_interrupted_code() {
    // `acp` waits on stdin, held open here; only the watchdog ends it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["--timeout", "1", "acp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run");
    let _stdin = child.stdin.take();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the watchdog never stopped the run");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(130));
}

#[test]
fn agent_mode_flag_values_are_not_the_command_and_refusals_are_envelopes() {
    // `--output json --help` is help, not a usage error.
    let out = rapid(&["--output", "json", "--help"], &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Every refusal is the envelope in JSON mode, with its own message.
    for (args, needle) in [
        (
            vec!["--output", "json", "--dry-run", "trust", "grant"],
            "no dry run",
        ),
        (
            vec!["--output", "json", "--non-interactive"],
            "--non-interactive",
        ),
        (
            vec!["--output", "json", "--timeout", "soon", "tools"],
            "--timeout",
        ),
        (
            vec!["--output", "json", "usage", "--since", "bogus"],
            "line above",
        ),
    ] {
        let out = rapid(&args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let doc: serde_json::Value =
            serde_json::from_str(stderr.lines().last().expect("line")).expect(&stderr);
        assert_eq!(doc["error"]["code"], "usage", "{args:?}");
        assert!(
            doc["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(needle)),
            "{args:?}: {stderr}"
        );
    }
}
