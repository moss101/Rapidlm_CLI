//! Tool-calling benchmark for the real `rapid` binary.
//!
//! A stateful scripted loopback provider stands in for a tool-hungry model
//! and drives `rapid exec` through hard, multi-step, multi-call scenarios.
//! Each scenario asserts correctness (disk effects, per-call results, turn
//! completion) and reports wall-clock/request metrics. These measure the CLI
//! mechanics — dispatch, gating, bounds, the turn loop — not model quality.
//!
//! Run: `cargo test -p rapid --test tool_call_bench -- --nocapture`

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

// ---------------------------------------------------------------------------
// Scripted loopback provider (stateful: one response per accepted request).
// ---------------------------------------------------------------------------

struct ScriptedServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

fn spawn_scripted_server(responses: Vec<String>) -> ScriptedServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    thread::spawn(move || {
        for (index, body) in responses.into_iter().enumerate() {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let raw = read_request(&mut stream);
            captured.lock().expect("capture lock").push(raw);
            let reason = if index % 2 == 0 { "OK" } else { "OK" };
            let response = format!(
                "HTTP/1.1 200 {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    ScriptedServer { addr, requests }
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
        let Some(split) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..split]
            .to_ascii_lowercase()
            .split("\r\n")
            .find_map(|line| {
                line.strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            });
        match length {
            Some(length) if buf.len() >= split + 4 + length => break,
            None => break,
            _ => {}
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------------------------------------------------------------------------
// SSE response builders.
// ---------------------------------------------------------------------------

/// One tool call as a chat-completions SSE stream (arguments JSON-escaped).
fn sse_tool_calls(calls: &[(&str, &str, &str)]) -> String {
    let mut body = String::new();
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        let wire = serde_json::json!({
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]}
            }]
        });
        body.push_str(&format!("data: {wire}\n\n"));
    }
    let finish = serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
    body.push_str(&format!("data: {finish}\n\n"));
    body.push_str("data: [DONE]\n\n");
    body
}

fn sse_terminal(text: &str) -> String {
    let wire = serde_json::json!({
        "choices": [{
            "delta": {"content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    });
    format!("data: {wire}\n\ndata: [DONE]\n\n")
}

// ---------------------------------------------------------------------------
// Real-binary harness: trusted project + config + run.
// ---------------------------------------------------------------------------

struct TrustedProject {
    home: PathBuf,
    project: PathBuf,
}

impl TrustedProject {
    fn new(tag: &str) -> Self {
        let home = temp_dir(&format!("{tag}-home"));
        let project = temp_dir(&format!("{tag}-proj"));
        std::fs::create_dir_all(project.join(".rapidlm")).expect("project marker");
        let root = std::fs::canonicalize(&project).expect("canon");
        let identity = ProjectIdentity::new(root, None).expect("identity");
        let store = ProjectTrustStore::open(home.join(".rapidlm/project-trust.json"));
        store
            .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
            .expect("grant trust");
        Self { home, project }
    }

    fn config(&self, base_url: &str) -> PathBuf {
        let path = self.home.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[models]\ndefault = \"bench\"\n\n[bench]\n\
                 provider = \"openai-compatible\"\n\
                 model = \"bench-model\"\n\
                 base_url = \"{base_url}\"\n\
                 api_key = \"bench-key\"\n\
                 \n\
                 [model.bench]\n\
                 provider = \"openai-compatible\"\n\
                 model = \"bench-model\"\n\
                 base_url = \"{base_url}\"\n\
                 api_key = \"bench-key\"\n"
            ),
        )
        .expect("write config");
        path
    }
}

impl Drop for TrustedProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-bench-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

struct BenchRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    requests: Vec<String>,
    wall: Duration,
}

fn run_bench(
    project: &Path,
    home: &Path,
    config_path: &Path,
    scenario: &str,
    prompt: &str,
    mode: &str,
) -> BenchRun {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", prompt])
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config_path)
        .env("RAPIDLM_PERMISSION_MODE", mode)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid");
    let wall = started.elapsed();
    let run = BenchRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        requests: Vec::new(),
        wall,
    };
    println!(
        "bench scenario={scenario} wall_ms={} exit={:?}",
        run.wall.as_millis(),
        run.code
    );
    run
}

fn config_with(server: &ScriptedServer, project: &TrustedProject) -> PathBuf {
    project.config(&format!("http://{}/v1", server.addr))
}

fn assert_no_loop_stop(run: &BenchRun, scenario: &str) {
    assert_ne!(
        run.code,
        Some(1),
        "{scenario}: turn must complete; stderr: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("repeated_tool_call"),
        "{scenario}: loop guard must not kill legitimate tool patterns: {}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------
// Scenarios.
// ---------------------------------------------------------------------------

#[test]
fn bench_a_multi_phase_agentic_task() {
    // Phase 1: two searches + two reads in ONE step (parallel dispatch).
    // Phase 2: three patches to three different files in one step.
    // Phase 3: shell verification (exit 0). Phase 4: confirm read. Terminal.
    let mut files = Vec::new();
    for index in 0..3 {
        files.push(format!("mod{index}.txt"));
    }
    let server = spawn_scripted_server(vec![
        sse_tool_calls(&[
            ("s1", "repo_search", r#"{"pattern":"target","head_limit":5}"#),
            ("s2", "repo_search", r#"{"pattern":"needle","head_limit":5}"#),
            ("r1", "repo_read", r#"{"path":"mod0.txt"}"#),
            ("r2", "repo_read", r#"{"path":"mod1.txt"}"#),
        ]),
        sse_tool_calls(&[
            ("p1", "workspace_patch", r#"{"path":"mod0.txt","old":"target","new":"patched-0"}"#),
            ("p2", "workspace_patch", r#"{"path":"mod1.txt","old":"target","new":"patched-1"}"#),
            ("p3", "workspace_patch", r#"{"path":"mod2.txt","old":"target","new":"patched-2"}"#),
        ]),
        sse_tool_calls(&[("sh1", "shell_exec", r#"{"argv":["./verify.sh"],"timeout_ms":30000}"#)]),
        sse_tool_calls(&[("v1", "repo_read", r#"{"path":"mod0.txt"}"#)]),
        sse_terminal("all three modules patched and verified"),
    ]);
    let project = TrustedProject::new("bench-a");
    for (index, file) in files.iter().enumerate() {
        std::fs::write(
            project.project.join(file),
            format!("fn target_{index}() {{}}\nneedle here\n"),
        )
        .expect("seed");
    }
    std::fs::write(
        project.project.join("verify.sh"),
        "#!/bin/sh\ngrep -q patched-0 mod0.txt && grep -q patched-1 mod1.txt && grep -q patched-2 mod2.txt\n",
    )
    .expect("seed script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            project.project.join("verify.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    let config = config_with(&server, &project);

    let run = run_bench(
        &project.project,
        &project.home,
        &config,
        "A_multi_phase",
        "patch all modules and verify",
        "acceptEdits",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("all three modules patched"));
    // Disk effects.
    for index in 0..3 {
        let content =
            std::fs::read_to_string(project.project.join(format!("mod{index}.txt"))).expect("file");
        assert!(content.contains(&format!("patched-{index}")), "{content}");
        assert!(!content.contains("target"), "old text gone: {content}");
    }
    // Requests: 5 steps. With full history replay each request carries the
    // tool messages of EVERY completed exchange so far (cumulative).
    assert_eq!(server.requests.lock().expect("r").len(), 5);
    let requests = server.requests.lock().expect("requests").clone();
    for (request, expected) in requests.iter().zip([0usize, 4, 7, 8, 9]) {
        let count = request.matches("\"role\":\"tool\"").count();
        assert_eq!(
            count, expected,
            "cumulative per-call tool messages on request: {}",
            request.lines().next().unwrap_or("")
        );
    }
    assert!(
        !requests.iter().any(|request| request.contains("tool results:")),
        "flat report must never appear"
    );
}

#[test]
fn bench_f_drained_notification_reaches_the_provider_request() {
    // Step 1 starts a background job; step 2 runs a foreground wait long
    // enough for the job to finish; the step-3 request must carry the drained
    // notification exchange (synthetic background_jobs call + tool result)
    // so the model learns the outcome without polling.
    let server = spawn_scripted_server(vec![
        sse_tool_calls(&[(
            "bg1",
            "shell_exec",
            r#"{"argv":["sh","-c","echo bg-done-marker"],"background":true,"timeout_ms":30000}"#,
        )]),
        sse_tool_calls(&[(
            "wait1",
            "shell_exec",
            r#"{"argv":["sleep","1"],"timeout_ms":30000}"#,
        )]),
        sse_terminal("background job finished"),
    ]);
    let project = TrustedProject::new("bench-f");
    let config = config_with(&server, &project);
    let run = run_bench(
        &project.project,
        &project.home,
        &config,
        "F_notification",
        "start a background job, wait a second, then tell me when it is done",
        "bypassPermissions",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("background job finished"), "{}", run.stdout);
    let requests = server.requests.lock().expect("requests");
    assert!(requests.len() >= 3, "expected at least three provider requests");
    let third = &requests[2];
    std::fs::write("/tmp/bench-f-third-request.txt", third).expect("dump");
    for (index, request) in requests.iter().enumerate() {
        let _ = std::fs::write(format!("/tmp/bench-f-all-{index}.txt"), request);
    }
    eprintln!("dumped {} requests", requests.len());
    eprintln!("dumped third request ({} bytes)", third.len());
    assert!(
        third.contains("background_jobs"),
        "synthetic background_jobs call missing from request"
    );
    assert!(
        third.contains("bg-done-marker"),
        "job output must reach the model without polling"
    );
}

#[test]
fn bench_b_sixteen_calls_at_the_per_step_cap() {
    // One step proposing the maximum 16 reads; every call must execute and
    // return its own result.
    let mut calls = Vec::new();
    for index in 0..16 {
        calls.push((
            format!("r{index}").leak() as &str,
            "repo_read",
            format!(r#"{{"path":"f{index}.txt"}}"#).leak() as &str,
        ));
    }
    let calls: Vec<(&str, &str, &str)> = calls;
    let server = spawn_scripted_server(vec![sse_tool_calls(&calls), sse_terminal("read them all")]);
    let project = TrustedProject::new("bench-b");
    for index in 0..16 {
        std::fs::write(project.project.join(format!("f{index}.txt")), format!("body {index}\n"))
            .expect("seed");
    }
    let config = config_with(&server, &project);
    let run = run_bench(
        &project.project,
        &project.home,
        &config,
        "B_cap16",
        "read all sixteen files",
        "acceptEdits",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("read them all"));
    let requests = server.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 2);
    let count = requests[1].matches("\"role\":\"tool\"").count();
    assert_eq!(count, 16, "every capped call gets its own result");
}

#[test]
fn bench_c_pagination_chain_over_a_long_file() {
    // 1200-line file: page 1 (lines 1-1000) exceeds the byte cap and must
    // honestly report its delivered window; page 2 (1001-1200) fits and
    // reaches EOF.
    let server = spawn_scripted_server(vec![
        sse_tool_calls(&[("r1", "repo_read", r#"{"path":"long.txt","offset":1,"limit":1000}"#)]),
        sse_tool_calls(&[("r2", "repo_read", r#"{"path":"long.txt","offset":1001,"limit":1000}"#)]),
        sse_terminal("pagination complete"),
    ]);
    let project = TrustedProject::new("bench-c");
    let body: Vec<String> = (1..=1200).map(|n| format!("line-{n}")).collect();
    std::fs::write(project.project.join("long.txt"), body.join("\n")).expect("seed");
    let config = config_with(&server, &project);
    let run = run_bench(
        &project.project,
        &project.home,
        &project.home.join("config.toml"),
        "C_pagination",
        "read the long file in pages",
        "acceptEdits",
    );
    let _ = config;
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("pagination complete"));
    let requests = server.requests.lock().expect("requests").clone();
    // The first page exceeds the byte cap, so the result must honestly name
    // the delivered window and the continuation line — never claim lines it
    // did not deliver.
    assert!(
        requests[1].contains("byte cap: lines 1-"),
        "honest delivered window missing: {}",
        requests[1]
    );
    assert!(
        requests[1].contains("continue at"),
        "continuation hint missing: {}",
        requests[1]
    );
    assert!(!requests[1].contains("line-1500"), "undelivered lines must not be claimed");
    // The second page fits and reaches EOF.
    assert!(requests[2].contains("line-1200"), "second page end present");
    assert!(requests[2].contains("[end of file"), "EOF marker reported");
}

#[test]
fn bench_d_verify_pattern_read_edit_reread() {
    // The core agentic verify loop: read, edit, re-read, edit again, verify
    // twice. The loop guard must tolerate repeated reads interleaved with
    // edits; only degenerate repetition (same call back-to-back) is a loop.
    let server = spawn_scripted_server(vec![
        sse_tool_calls(&[("r1", "repo_read", r#"{"path":"notes.txt"}"#)]),
        sse_tool_calls(&[("p1", "workspace_patch", r#"{"path":"notes.txt","old":"alpha","new":"beta"}"#)]),
        sse_tool_calls(&[("r2", "repo_read", r#"{"path":"notes.txt"}"#)]),
        sse_tool_calls(&[("p2", "workspace_patch", r#"{"path":"notes.txt","old":"beta","new":"gamma"}"#)]),
        sse_tool_calls(&[("r3", "repo_read", r#"{"path":"notes.txt"}"#)]),
        sse_tool_calls(&[("r4", "repo_read", r#"{"path":"notes.txt"}"#)]),
        sse_terminal("verified through both edits"),
    ]);
    let project = TrustedProject::new("bench-d");
    std::fs::write(project.project.join("notes.txt"), "alpha\n").expect("seed");
    let config = config_with(&server, &project);
    let run = run_bench(
        &project.project,
        &project.home,
        &config,
        "D_verify_pattern",
        "edit notes twice with verification reads",
        "acceptEdits",
    );
    assert_no_loop_stop(&run, "D_verify_pattern");
    assert!(run.stdout.contains("verified through both edits"), "{}", run.stdout);
    assert_eq!(
        std::fs::read_to_string(project.project.join("notes.txt")).expect("read"),
        "gamma\n",
        "both edits must have landed"
    );
}

#[test]
fn bench_e_substantial_patch_payload() {
    // A realistic code rewrite: old block and new block of a few KB each.
    // The turn-layer 8 KiB argument budget must not make the advertised
    // per-text bounds unreachable.
    let old_block: String = (0..60).map(|i| format!("old line {i:03} with some code\n")).collect();
    let new_block: String = (0..60).map(|i| format!("new line {i:03} refactored\n")).collect();
    let arguments = format!(
        r#"{{"path":"big.rs","old":{},"new":{}}}"#,
        serde_json::json!(old_block),
        serde_json::json!(new_block)
    );
    let arguments_leak: &str = Box::leak(arguments.into_boxed_str());
    let server = spawn_scripted_server(vec![
        sse_tool_calls(&[("p1", "workspace_patch", arguments_leak)]),
        sse_terminal("refactor applied"),
    ]);
    let project = TrustedProject::new("bench-e");
    std::fs::write(project.project.join("big.rs"), &old_block).expect("seed");
    let config = config_with(&server, &project);
    let run = run_bench(
        &project.project,
        &project.home,
        &config,
        "E_patch_payload",
        "apply the refactor to big.rs",
        "acceptEdits",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("refactor applied"));
    let content = std::fs::read_to_string(project.project.join("big.rs")).expect("read");
    assert!(content.contains("new line 059 refactored"), "{content}");
    assert!(!content.contains("old line 000"), "{content}");
}
