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

/// A project directory with a `.git` marker plus a trust record for it in the
/// temp home's catalog, mirroring the interactive trust grant.
fn trusted_project(home: &Path, project: &Path) {
    std::fs::create_dir_all(project.join(".git")).expect("git marker");
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

    assert_eq!(code, Some(1), "a tool-failure stop exits non-zero");
    // Per-call stderr line with the tool name and the failing outcome.
    assert!(stderr.contains("tool shell_exec:"), "per-call line missing: {stderr}");
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

    let (code, _stdout, stderr) =
        run_exec(&project, &home, &config, &["--verbose", "do one step"]);

    assert_eq!(code, Some(0), "empty final after committed work exits 0: {stderr}");
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

    let (code, stdout, stderr) =
        run_exec(&project, &home, &config, &["--verbose", "say pong"]);

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
