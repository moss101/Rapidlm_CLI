//! Cross-process proof that `.rapidlm/goal.json` survives concurrent
//! writers without losing an update — the actual production binary, two
//! real OS processes, racing for real.
//!
//! `GoalHost::update`'s in-process, multi-threaded tests (`goal_host.rs`)
//! already prove the underlying `GoalLock` mechanism serializes correctly;
//! this file proves the same property holds when the two writers are
//! genuinely separate `rapid` processes, not just separate threads sharing
//! one address space — the scenario this task's own spec calls out
//! explicitly (a TUI turn and a concurrent headless `rapid exec`, or two
//! headless turns, against the same project).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// One scripted terminal response, with a controllable response delay to
/// deterministically widen the race window: `rapid exec`'s own turn spans
/// from reading the active goal id to accruing usage at the end, and that
/// window is exactly as long as this response takes to arrive. A fixed,
/// generous delay (300ms) makes two concurrently-launched `rapid exec`
/// processes' accrual windows reliably overlap regardless of ordinary
/// process-spawn/connect jitter (single-digit milliseconds on a real
/// machine) — not a probabilistic "hope it races," an engineered one.
fn terminal_body(prompt_tokens: u64, completion_tokens: u64) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":"done"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{prompt_tokens},"completion_tokens":{completion_tokens}}}}}"#
    )
}

fn config_doc(base_url: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\
         \n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"scripted-key\"\n"
    )
}

/// Accepts exactly one connection, waits `delay`, then replies with `body`.
fn spawn_scripted_server(body: String, delay: Duration) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = read_request(&mut stream);
        thread::sleep(delay);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    });
    addr
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        if text.contains("\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-goalrace-{name}-{}-{}",
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
/// `exec_diagnosability.rs`'s own helper of the same shape.
fn trusted_project(home: &Path, project: &Path) {
    std::fs::create_dir_all(project).expect("project");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    for (key, value) in [
        ("user.email", "goalrace@example.com"),
        ("user.name", "goalrace"),
    ] {
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

fn write_config(path: &Path, addr: std::net::SocketAddr) {
    std::fs::write(path, config_doc(&format!("http://{addr}/v1"))).expect("config");
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

/// Spawn (but do not wait for) a real `rapid exec` child process against
/// `project`, using `config` for its model backend.
fn spawn_exec(project: &Path, home: &Path, config: &Path, prompt: &str) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", prompt])
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env("RAPIDLM_RETRY_BASE_MS", "1")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .spawn()
        .expect("spawn rapid exec")
}

/// The full text of `.rapidlm/goal.json`, parsed.
fn read_goal_snapshot(project: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(project.join(".rapidlm").join("goal.json"))
        .expect("goal.json must exist after both turns");
    serde_json::from_str(&raw).expect("goal.json must be valid JSON — no torn/corrupt write")
}

/// Writer count for the concurrent-`rapid exec` race below. Two processes
/// alone under-covers the actual vulnerable window (a real `GoalHost::load`
/// plus a JSON reserialize plus `atomic_write` is a sub-millisecond-to-low-
/// single-digit-millisecond operation — far shorter than ordinary inter-
/// process scheduling jitter between two heavyweight `rapid` invocations),
/// so two processes racing rarely land *inside* that narrow window by
/// chance alone. Eight processes turns this into `C(8,2) = 28` pairwise
/// chances for two of them to overlap inside it instead of just one —
/// enough to make the race reliably observable (confirmed empirically
/// during this test's own revert-cycle: reverting `accrue_turn_usage` to
/// bypass `GoalHost::update`'s lock reproducibly lost at least one of the
/// 8 writers across repeated runs, where the same probe against only 2
/// writers did not reliably reproduce anything).
const WRITERS: u64 = 8;

#[test]
fn concurrent_rapid_exec_processes_all_accrue_usage_to_the_same_goal() {
    let home = temp_dir("home");
    let project = home.join("project");
    trusted_project(&home, &project);

    // Setup: create the goal synchronously, before any race participant
    // starts — not itself part of the timed race.
    let (code, _out, err) = run_goal(
        &project,
        &home,
        &["create", "ship it", "--max-tokens", "1000000"],
    );
    assert_eq!(code, Some(0), "goal create must succeed: {err}");

    // One scripted server per writer, each replying to exactly one request
    // after the same deliberate delay — synchronizing every writer's
    // load-...-accrue window to start at nearly the same instant, without
    // needing any test-only hook inside production code. Token counts are
    // `10 * (i + 1)` so the expected sum is easy to compute and each
    // writer's own contribution is individually distinguishable if a
    // failure needs to be diagnosed.
    let delay = Duration::from_millis(300);
    let mut configs = Vec::new();
    let mut expected_tokens: u64 = 0;
    for i in 0..WRITERS {
        let tokens = 10 * (i + 1);
        expected_tokens += tokens;
        let addr = spawn_scripted_server(terminal_body(tokens, 0), delay);
        let config = home.join(format!("config-{i}.toml"));
        write_config(&config, addr);
        configs.push(config);
    }

    // Launch every writer concurrently — never `.output()` (which would
    // block on one before the next even starts) — then wait for all.
    let children: Vec<_> = configs
        .iter()
        .enumerate()
        .map(|(i, config)| spawn_exec(&project, &home, config, &format!("turn {i}")))
        .collect();
    for (i, mut child) in children.into_iter().enumerate() {
        let status = child
            .wait()
            .unwrap_or_else(|err| panic!("wait for turn {i}: {err}"));
        assert!(status.success(), "turn {i} must exit cleanly");
    }

    let snapshot = read_goal_snapshot(&project);
    let usage = &snapshot["usage"];
    assert_eq!(
        usage["turns"], WRITERS,
        "every writer must be counted, not just whichever process saved last: {snapshot}"
    );
    assert_eq!(
        usage["tokens"], expected_tokens,
        "every writer's tokens must survive, not just one writer's: {snapshot}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
