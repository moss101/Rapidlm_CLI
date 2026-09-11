//! End-to-end diagnosability coverage for `rapid exec` against a scripted
//! loopback OpenAI-compatible provider:
//!
//! 1. a tool-failure stop names the failing tool and the underlying error on
//!    stderr, with one line per tool call;
//! 2. a run whose only defect is an empty final model response — after real
//!    committed tool work — exits 0;
//! 3. an untrusted project warns at start that workspace tools are disabled
//!    and how to enable them;
//! 4. a provider rejection is retried (visible as `attempt=1` in verbose
//!    output) instead of failing the turn on first occurrence;
//! 5. `rapid exec --help` prints exec-specific usage.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

/// Terminal text response (same shape as configured_model_integration).
const TERMINAL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hello from scripted model"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

/// Terminal response with empty content: the provider billed the request but
/// returned no text.
const EMPTY_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":0}}"#;

/// One shell_exec tool call proposing an argv that cannot spawn.
const SHELL_FAIL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"shell_exec","arguments":"{\"argv\":[\"definitely-not-a-real-cmd-xyz\"]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

/// One shell_exec tool call proposing a command that succeeds.
const SHELL_OK_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"shell_exec","arguments":"{\"argv\":[\"true\"]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

/// Tool call with an explicit argv (used to drive git operations).
fn shell_call_body(id: &str, argv: &[&str]) -> String {
    tool_call_body(
        id,
        "shell_exec",
        &format!(
            "{{\\\"argv\\\":[{0}]}}",
            argv.iter()
                .map(|a| format!("\\\"{a}\\\""))
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}

/// Tool call with an explicit raw tool name and pre-escaped arguments JSON.
fn tool_call_body(id: &str, tool: &str, arguments_json: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"{id}","type":"function","function":{{"name":"{tool}","arguments":"{arguments_json}"}}}}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":3,"completion_tokens":5}}}}"#
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

/// One scripted response per accepted connection; requests are captured.
struct ScriptedServer {
    addr: std::net::SocketAddr,
}

fn spawn_scripted_server(responses: Vec<(u16, String)>) -> ScriptedServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let _ = read_request(&mut stream);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    ScriptedServer { addr }
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
        "rapidlm-execdiag-{name}-{}-{}",
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

/// A project directory with a real git repo (commit identity configured) plus
/// a trust record for it in the temp home's catalog, mirroring the
/// interactive trust grant.
fn trusted_project(home: &Path, project: &Path) {
    std::fs::create_dir_all(project).expect("project");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    for (key, value) in [("user.email", "bench@example.com"), ("user.name", "bench")] {
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

fn write_config(home: &Path, addr: std::net::SocketAddr) -> PathBuf {
    let config = home.join("config.toml");
    std::fs::write(&config, config_doc(&format!("http://{addr}/v1"))).expect("config");
    config
}

/// Run the real binary's `exec` with a prompt assembled from `args`.
fn run_exec(
    project: &Path,
    home: &Path,
    config: &Path,
    extra_args: &[&str],
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    let mut all: Vec<String> = vec!["exec".to_owned()];
    all.extend(extra_args.iter().map(|s| s.to_string()));
    command
        .args(&all)
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env("RAPIDLM_RETRY_BASE_MS", "1")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL");
    let output = command.output().expect("run rapid");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn tool_failure_names_failing_tool_and_error_on_stderr() {
    let home = temp_dir("toolfail-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    let server = spawn_scripted_server(vec![(200, SHELL_FAIL_BODY.to_owned())]);
    let config = write_config(&home, server.addr);

    let (code, _stdout, stderr) = run_exec(
        &project,
        &home,
        &config,
        &["--verbose", "run the impossible command"],
    );

    // JsonlExitCode::Runtime: a tool-failure stop carries no provider-
    // classified FailureCause, so it falls into the generic Runtime bucket.
    assert_eq!(code, Some(5), "a tool-failure stop exits non-zero");
    // Per-call stderr line with the tool name and the failing outcome.
    assert!(
        stderr.contains("tool shell_exec:"),
        "per-call line missing: {stderr}"
    );
    // The terminal failure names the tool and the underlying error instead of
    // the bare `tool_failed`.
    assert!(
        stderr.contains("failing tool: shell_exec"),
        "terminal message does not name the failing tool: {stderr}"
    );
    assert!(stderr.contains("tool step failed"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn jsonl_flag_reports_a_failed_turn_with_no_assistant_message_record() {
    let home = temp_dir("jsonl-fail-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    let server = spawn_scripted_server(vec![(200, SHELL_FAIL_BODY.to_owned())]);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(
        &project,
        &home,
        &config,
        &["--jsonl", "run the impossible command"],
    );

    assert_eq!(code, Some(5), "stdout: {stdout} stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    // No assistant.message record: the turn never produced a real result.
    assert_eq!(lines.len(), 2, "schema + session.finished only: {stdout}");
    let schema: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON line");
    assert_eq!(schema["type"], "rapid.schema");
    let finished: serde_json::Value = serde_json::from_str(lines[1]).expect("valid JSON line");
    assert_eq!(finished["type"], "session.finished");
    assert_eq!(finished["data"]["exit_code"], 5);
    // The tool-failure diagnostic still reaches stderr unchanged.
    assert!(stderr.contains("failing tool: shell_exec"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn json_schema_flag_prints_the_validated_result_instead_of_the_summary() {
    let home = temp_dir("jsonschema-ok-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    let schema_path = home.join("schema.json");
    std::fs::write(
        &schema_path,
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
    )
    .expect("write schema");
    let responses = vec![
        (
            200,
            tool_call_body("call_1", "emit_structured_result", r#"{\"answer\":\"42\"}"#),
        ),
        (200, TERMINAL_BODY.to_owned()),
    ];
    let server = spawn_scripted_server(responses);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(
        &project,
        &home,
        &config,
        &[
            "--json-schema",
            schema_path.to_str().expect("utf-8 path"),
            "return the structured answer",
        ],
    );

    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(
        stdout.trim(),
        r#"{"answer":"42"}"#,
        "stdout must be the validated result, not the model's own summary text"
    );
    assert!(
        !stdout.contains("hello from scripted model"),
        "the model's own terminal text must not leak into structured-output stdout: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn jsonl_flag_writes_versioned_protocol_records_and_keeps_diagnostics_on_stderr() {
    let home = temp_dir("jsonl-ok-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(&project, &home, &config, &["--jsonl", "say hello"]);

    assert_eq!(code, Some(0), "stdout: {stdout} stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "schema + assistant.message + session.finished: {stdout}"
    );

    let schema: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON line");
    assert_eq!(schema["type"], "rapid.schema");
    assert_eq!(schema["schema"], 1);
    assert_eq!(schema["data"]["version"], 1);

    let message: serde_json::Value = serde_json::from_str(lines[1]).expect("valid JSON line");
    assert_eq!(message["type"], "assistant.message");
    assert!(message["session_id"].is_string(), "{message}");
    assert!(
        message["data"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("hello from scripted model"),
        "{message}"
    );

    let finished: serde_json::Value = serde_json::from_str(lines[2]).expect("valid JSON line");
    assert_eq!(finished["type"], "session.finished");
    assert_eq!(finished["data"]["exit_code"], 0);
    assert_eq!(
        finished["session_id"], message["session_id"],
        "every record in one run shares the same session id"
    );

    // Diagnostics (the human-oriented "tokens used" line) still go to
    // stderr — stdout stays protocol-only either way.
    assert!(stderr.contains("tokens used:"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn json_schema_flag_fails_typed_when_the_model_never_calls_the_synthetic_tool() {
    let home = temp_dir("jsonschema-missing-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    let schema_path = home.join("schema.json");
    std::fs::write(
        &schema_path,
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}"#,
    )
    .expect("write schema");
    // The model just answers in plain text and never calls the synthetic
    // tool at all — the constrained-output contract was not met.
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(
        &project,
        &home,
        &config,
        &[
            "--json-schema",
            schema_path.to_str().expect("utf-8 path"),
            "return the structured answer",
        ],
    );

    // JsonlExitCode::Runtime: the turn otherwise "succeeded" but never
    // produced the promised structured result.
    assert_eq!(code, Some(5), "stdout: {stdout} stderr: {stderr}");
    assert!(
        stderr.contains("never called the synthetic tool"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn empty_final_response_after_committed_work_exits_zero() {
    let home = temp_dir("emptyexit-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    trusted_project(&home, &project);
    // Step 1: shell_exec "true" commits a tool effect. Step 2: the final
    // response stays empty through every bounded retry (3 turn attempts x
    // (2 retries + 1) = 9 requests).
    let mut responses = vec![(200, SHELL_OK_BODY.to_owned())];
    for _ in 0..9 {
        responses.push((200, EMPTY_BODY.to_owned()));
    }
    let server = spawn_scripted_server(responses);
    let config = write_config(&home, server.addr);

    let (code, _stdout, stderr) = run_exec(&project, &home, &config, &["--verbose", "do one step"]);

    assert_eq!(
        code,
        Some(0),
        "empty final after committed work exits 0: {stderr}"
    );
    assert!(
        stderr.contains("empty_response"),
        "diagnostics should name the empty response: {stderr}"
    );
    assert!(
        stderr.contains("tool call(s)"),
        "exit-0 note should report the committed tool calls: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn untrusted_project_warns_that_tools_are_disabled() {
    let home = temp_dir("untrusted-home");
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".git")).expect("project");
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(&project, &home, &config, &["say pong"]);

    assert_eq!(code, Some(0));
    assert!(stdout.contains("hello from scripted model"));
    assert!(
        stderr.contains("workspace tools are disabled"),
        "start warning missing: {stderr}"
    );
    assert!(
        stderr.contains("RAPIDLM_PERMISSION_MODE"),
        "warning should name the permission-mode lever: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn provider_rejection_is_retried_and_visible_in_verbose() {
    let home = temp_dir("retry-home");
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".git")).expect("project");
    let server = spawn_scripted_server(vec![
        (
            400,
            r#"{"error":{"message":"capacity","type":"temporary"}}"#.to_owned(),
        ),
        (200, TERMINAL_BODY.to_owned()),
    ]);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(&project, &home, &config, &["--verbose", "say pong"]);

    assert_eq!(code, Some(0), "a retried rejection must recover: {stderr}");
    assert!(stdout.contains("hello from scripted model"));
    assert!(
        stderr.contains("attempt=1"),
        "verbose output should show the retry attempt: {stderr}"
    );
    assert!(
        stderr.contains("failed:rejected"),
        "verbose output should classify the first failure: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn exec_help_prints_exec_specific_usage() {
    let home = temp_dir("help-home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).expect("project");
    let config = home.join("config.toml");

    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["exec", "--help"])
        .current_dir(&project)
        .env("HOME", &home)
        .env("RAPIDLM_CONFIG", &config)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL");
    let output = command.output().expect("run rapid");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("rapid exec <prompt>"), "{stdout}");
    assert!(stdout.contains("--verbose"), "{stdout}");
    assert!(stdout.contains("RAPIDLM_PERMISSION_MODE"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn stderr_stays_complete_when_the_agent_git_adds_the_log_file() {
    // The triggering shape of the observed mid-run stderr silence: a
    // multi-call run where the agent stages and commits the workspace —
    // including the process's own stderr redirect target, which lives inside
    // it — while stderr is held open. Every diagnostic line must survive
    // through process exit: one argv + one outcome line per shell call,
    // `turn outcome=`, and `tokens used:`.
    let home = temp_dir("gitlog-home");
    let project = home.join("project");
    trusted_project(&home, &project);
    std::fs::write(project.join("work.txt"), "seed content\n").expect("work file");
    let responses = vec![
        (200, shell_call_body("call_1", &["git", "add", "-A"])),
        (
            200,
            shell_call_body("call_2", &["git", "commit", "-m", "wip"]),
        ),
        (200, shell_call_body("call_3", &["false"])),
        (200, shell_call_body("call_4", &["git", "add", "-A"])),
        (200, TERMINAL_BODY.to_owned()),
    ];
    let server = spawn_scripted_server(responses);
    let config = write_config(&home, server.addr);

    // stderr is redirected to a file INSIDE the workspace, exactly like the
    // benchmark harness redirect that exposed the anomaly.
    let err_path = project.join("err.log");
    let err_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&err_path)
        .expect("open stderr log");

    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["exec", "--verbose", "work through the steps"])
        .current_dir(&project)
        .env("HOME", &home)
        .env("RAPIDLM_CONFIG", &config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env("RAPIDLM_RETRY_BASE_MS", "1")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(err_file));
    let output = command.output().expect("run rapid");
    let code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    drop(output);
    let stderr = std::fs::read_to_string(&err_path).expect("read stderr log");

    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("hello from scripted model"), "{stdout}");
    // Four shell calls, each fully traced with its outcome.
    assert_eq!(
        stderr.matches("tool shell_exec: argv=").count(),
        4,
        "{stderr}"
    );
    // add (0), commit (0), false (1), add (0).
    assert_eq!(
        stderr.matches("tool shell_exec: ok (exit 0").count(),
        3,
        "{stderr}"
    );
    assert!(
        stderr.contains("tool shell_exec: ok (exit 1"),
        "the failing command's handled outcome is still traced: {stderr}"
    );
    // The complete terminal tail.
    assert!(stderr.contains("turn outcome=succeeded"), "{stderr}");
    assert!(stderr.contains("tokens used:"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unknown_tool_proposal_is_model_correctable_not_fatal() {
    // Free-tier models sometimes propose a tool name from a different CLI
    // (`read`). The proposal must come back as a handled per-call failure
    // the model can correct — never a dead turn.
    let home = temp_dir("unknowntool-home");
    let project = home.join("project");
    trusted_project(&home, &project);
    let responses = vec![
        (200, tool_call_body("call_1", "read", "{}")),
        (200, SHELL_OK_BODY.to_owned()),
        (200, TERMINAL_BODY.to_owned()),
    ];
    let server = spawn_scripted_server(responses);
    let config = write_config(&home, server.addr);

    let (code, stdout, stderr) = run_exec(
        &project,
        &home,
        &config,
        &["--verbose", "read a file then act"],
    );

    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("hello from scripted model"));
    assert!(
        stderr.contains("tool read: failed (unknown tool `read`"),
        "the unknown-tool proposal should trace as a handled failure: {stderr}"
    );
    assert!(
        stderr.contains("tool shell_exec: ok (exit 0"),
        "the model's corrected follow-up call should run: {stderr}"
    );
    assert!(stderr.contains("turn outcome=succeeded"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}
